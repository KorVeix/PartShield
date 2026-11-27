//! Conjuntos de copia («un disco, un conjunto») y su restauración, sin GTK.
//!
//! Al copiar un disco desde el modo sencillo se escribe, junto a las copias,
//! un `<disco>-<fecha>.set.json` que dice de qué disco eran, dónde está su
//! tabla de particiones y qué informe corresponde a cada partición. Con él,
//! restaurar es una sola acción: tabla, y después cada partición.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{anyhow, bail, Context, Result};
use partshield_core::cmd::{Cmd, Session};
use partshield_core::detect::devices::Disk;
use partshield_core::raw;
use partshield_core::report::load_report_value;
use partshield_core::send::layout::apply_fs_uuid;
use partshield_core::send::multi::restore_many;
use partshield_core::types::{Digest, SubvolArtifact};
use serde::{Deserialize, Serialize};

use crate::model::{is_mountpoint, subvol_label, DEST_FSTYPES, RUN_DIR};

pub const SET_SUFFIX: &str = ".set.json";
/// Carpeta de las copias en el disco de destino.
pub const COPIES_DIR: &str = "partshield-copias";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SetVerdict {
    Valid,
    Reduced,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetDisk {
    pub path: String,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPart {
    /// Posición en la tabla de particiones (desde 0), en el orden de
    /// `sfdisk --json`. `None` si no era una partición de la tabla.
    pub index: Option<usize>,
    pub device: String,
    /// «Windows (NTFS) · 295 GiB».
    pub title: String,
    pub size: u64,
    pub fstype: Option<String>,
    /// Informe de la copia, junto al conjunto. `None` si la copia falló.
    pub report: Option<String>,
    pub verdict: SetVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Subvolumen por defecto del Btrfs, si no era el nivel superior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_subvolume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupSet {
    pub version: u32,
    /// Fecha y hora locales, RFC 3339.
    pub created: String,
    pub disk: SetDisk,
    /// Volcado `sfdisk --dump`, junto al conjunto. Solo si se copió el
    /// disco entero.
    pub table: Option<String>,
    pub parts: Vec<SetPart>,
}

impl BackupSet {
    /// El peor veredicto de sus partes.
    pub fn verdict(&self) -> SetVerdict {
        self.parts
            .iter()
            .map(|p| p.verdict)
            .max()
            .unwrap_or(SetVerdict::Invalid)
    }

    /// Por qué no se puede restaurar como disco entero, si no se puede.
    pub fn not_restorable(&self) -> Option<String> {
        if self.table.is_none() {
            return Some("no se copió el disco entero: falta la tabla de particiones".into());
        }
        if let Some(p) = self.parts.iter().find(|p| p.report.is_none()) {
            return Some(format!("la copia de «{}» falló", p.title));
        }
        if let Some(p) = self.parts.iter().find(|p| p.index.is_none()) {
            return Some(format!("«{}» no es una partición de la tabla", p.title));
        }
        None
    }

    pub fn save(&self, dir: &Path, name: &str) -> Result<PathBuf> {
        let path = dir.join(format!("{name}{SET_SUFFIX}"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("creando {} (no se sobrescribe)", path.display()))?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        Ok(path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("leyendo {}", path.display()))?;
        let set: Self = serde_json::from_str(&text)
            .with_context(|| format!("{} no es un conjunto de copia", path.display()))?;
        if set.version != 1 {
            bail!(
                "{} es de una versión más nueva de PartShield ({})",
                path.display(),
                set.version
            );
        }
        Ok(set)
    }
}

/// Posición de `device` en la tabla de particiones de `disk`.
pub fn table_index(disk: &Path, device: &Path) -> Option<usize> {
    let mut s = Session::new(false);
    partition_nodes(&mut s, disk)
        .ok()?
        .iter()
        .position(|n| n == device)
}

/// Nodos de las particiones de `disk`, en el orden de la tabla.
fn partition_nodes(s: &mut Session, disk: &Path) -> Result<Vec<PathBuf>> {
    let json = s.run(Cmd::new("sfdisk").arg("--json").arg(disk))?;
    let v: serde_json::Value = serde_json::from_str(&json)?;
    Ok(v["partitiontable"]["partitions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["node"].as_str().map(PathBuf::from))
        .collect())
}

// ----------------------------------------------------------------- buscar

/// Un conjunto encontrado en un disco de copias.
#[derive(Debug, Clone)]
pub struct FoundSet {
    pub set: BackupSet,
    pub path: PathBuf,
    /// Disco físico donde está la copia: nunca se ofrece como destino.
    pub on_disk: PathBuf,
}

/// Busca conjuntos en los discos que no son el USB de arranque. Los
/// volúmenes sin montar se montan en solo lectura bajo /run/partshield y se
/// quedan montados si tienen copias (hacen falta para restaurar).
pub fn find_sets(disks: &[Disk]) -> Vec<FoundSet> {
    let mut s = Session::new(false);
    let mut out = Vec::new();
    for d in disks {
        if d.volumes
            .iter()
            .any(|v| v.is_live_medium() || v.is_live_system())
        {
            continue;
        }
        for v in &d.volumes {
            if !v
                .fstype
                .as_deref()
                .is_some_and(|f| DEST_FSTYPES.contains(&f))
            {
                continue;
            }
            let mounted = v.mountpoints.iter().find(|m| m.starts_with('/'));
            let (root, ours) = match mounted {
                Some(m) => (PathBuf::from(m), false),
                None => {
                    let name = v
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let mnt = PathBuf::from(RUN_DIR).join("copias").join(name);
                    if std::fs::create_dir_all(&mnt).is_err() {
                        continue;
                    }
                    if !is_mountpoint(&mut s, &mnt)
                        && s.run(Cmd::new("mount").arg("-o").arg("ro").arg(&v.path).arg(&mnt))
                            .is_err()
                    {
                        continue;
                    }
                    (mnt, true)
                }
            };
            let found = sets_in(&root.join(COPIES_DIR), &d.path);
            if found.is_empty() && ours {
                let _ = s.run_unchecked(&Cmd::new("umount").arg(&root));
            }
            out.extend(found);
        }
    }
    out.sort_by(|a, b| b.set.created.cmp(&a.set.created));
    out
}

fn sets_in(dir: &Path, on_disk: &Path) -> Vec<FoundSet> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(SET_SUFFIX))
        .filter_map(|path| {
            BackupSet::load(&path).ok().map(|set| FoundSet {
                set,
                path,
                on_disk: on_disk.to_path_buf(),
            })
        })
        .collect()
}

/// Discos donde se puede restaurar `found`: ni el USB de arranque ni el
/// disco que guarda la copia, y al menos del tamaño del original.
pub fn restore_targets<'a>(disks: &'a [Disk], found: &FoundSet, loops: bool) -> Vec<&'a Disk> {
    disks
        .iter()
        .filter(|d| {
            !d.volumes
                .iter()
                .any(|v| v.is_live_medium() || v.is_live_system())
        })
        .filter(|d| d.path != found.on_disk)
        .filter(|d| loops || d.kind != "loop")
        .filter(|d| d.size >= found.set.disk.size)
        .collect()
}

/// Lo que hay que teclear para confirmar que se borra `disk`: los cuatro
/// últimos caracteres de su número de serie, o BORRAR si no tiene.
pub fn confirmation_word(disk: &Disk) -> String {
    match disk.serial.as_deref().map(str::trim) {
        Some(s) if s.chars().count() >= 4 => {
            let n = s.chars().count();
            s.chars().skip(n - 4).collect()
        }
        _ => "BORRAR".into(),
    }
}

// --------------------------------------------------------------- restaurar

pub enum RestoreEvent {
    /// Empieza un paso: «Tabla de particiones», «Windows (NTFS) · 295 GiB»…
    Step(usize, String),
    Command(String),
    /// Todo terminó. `Err` con el motivo si algo falló; lo ya escrito queda.
    Finished(Result<Vec<String>, String>),
}

/// Pasos de una restauración, para la barra: la tabla y cada partición.
pub fn restore_steps(set: &BackupSet) -> usize {
    1 + set.parts.len()
}

/// Restaura el conjunto de `set_path` en `target`, que se borra entero.
/// `part_devices` sustituye a los nodos de partición del kernel (pruebas
/// con loops sin `partscan`).
pub fn run_restore(
    set_path: &Path,
    target: &Path,
    part_devices: Option<Vec<PathBuf>>,
    events: Sender<RestoreEvent>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let fwd = {
        let events = events.clone();
        std::thread::spawn(move || {
            for c in rx {
                let _ = events.send(RestoreEvent::Command(c));
            }
        })
    };
    let mut s = Session::new(false);
    s.progress = Some(tx);
    // Solo para pruebas con loops sin `partscan`, y solo en el sandbox
    // (donde no se puede escribir más que en loops).
    let part_devices = part_devices.or_else(|| {
        let v = std::env::var("PARTSHIELD_RESTORE_PARTS").ok()?;
        partshield_core::cmd::sandboxed().then(|| v.split(',').map(PathBuf::from).collect())
    });
    let r = restore_set(&mut s, set_path, target, part_devices, &events);
    drop(s);
    let _ = fwd.join();
    let _ = events.send(RestoreEvent::Finished(r.map_err(|e| format!("{e:#}"))));
}

/// Qué hay que escribir en una partición.
enum PartPlan {
    Raw(
        Box<(
            partshield_core::types::StreamRef,
            partshield_core::types::RawImage,
        )>,
    ),
    Btrfs {
        items: Vec<SubvolArtifact>,
        uuid: String,
        label: Option<String>,
        default_subvolume: Option<String>,
    },
}

fn restore_set(
    s: &mut Session,
    set_path: &Path,
    target: &Path,
    part_devices: Option<Vec<PathBuf>>,
    events: &Sender<RestoreEvent>,
) -> Result<Vec<String>> {
    let set = BackupSet::load(set_path)?;
    if let Some(why) = set.not_restorable() {
        bail!("este conjunto no se puede restaurar como disco entero: {why}");
    }
    let dir = set_path.parent().unwrap_or(Path::new("."));
    let table = dir.join(set.table.as_deref().unwrap_or_default());
    let dump = std::fs::read_to_string(&table)
        .with_context(|| format!("leyendo la tabla de particiones {}", table.display()))?;

    // Antes de escribir nada: todos los informes se leen y todos los
    // archivos de la copia tienen el hash esperado.
    let mut plans = Vec::new();
    for p in &set.parts {
        let report = dir.join(p.report.as_deref().unwrap_or_default());
        let plan = load_part(&report, p)
            .with_context(|| format!("«{}»: no se ha escrito nada en el disco", p.title))?;
        plans.push(plan);
    }

    partshield_core::cmd::guard_path(target)?;
    raw::ensure_not_mounted(s, target)?;
    let size: u64 = s
        .run(Cmd::new("blockdev").arg("--getsize64").arg(target))?
        .trim()
        .parse()
        .context("tamaño del disco de destino")?;
    if size < set.disk.size {
        bail!(
            "{} mide {size} bytes y el disco original {}: no cabe",
            target.display(),
            set.disk.size
        );
    }

    // 1. Tabla de particiones. En un disco mayor, sfdisk recalcula el final.
    let _ = events.send(RestoreEvent::Step(0, "Tabla de particiones".into()));
    let dump: String = dump
        .lines()
        .filter(|l| size == set.disk.size || !l.starts_with("last-lba:"))
        .map(|l| format!("{l}\n"))
        .collect();
    s.pipe_in(
        Cmd::new("sfdisk").arg("--wipe").arg("always").arg(target),
        |stdin| Ok(stdin.write_all(dump.as_bytes())?),
    )
    .context("escribiendo la tabla de particiones")?;
    let _ = s.run_unchecked(&Cmd::new("partx").arg("-u").arg(target));
    let _ = s.run_unchecked(&Cmd::new("udevadm").arg("settle"));

    let devices = match part_devices {
        Some(d) => d,
        None => {
            let nodes = partition_nodes(s, target)?;
            for _ in 0..50 {
                if nodes.iter().all(|n| n.exists()) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            nodes
        }
    };

    // 2. Cada partición.
    let mut done = vec![format!(
        "Tabla de particiones escrita en {}",
        target.display()
    )];
    for (i, (p, plan)) in set.parts.iter().zip(plans).enumerate() {
        let _ = events.send(RestoreEvent::Step(i + 1, p.title.clone()));
        let dev = p
            .index
            .and_then(|ix| devices.get(ix))
            .ok_or_else(|| anyhow!("«{}»: el disco de destino no tiene esa partición", p.title))?;
        if !dev.exists() {
            bail!(
                "«{}»: no aparece {} tras escribir la tabla",
                p.title,
                dev.display()
            );
        }
        match plan {
            PartPlan::Raw(b) => {
                let (stream, image) = *b;
                let d = raw::restore_to(s, &stream, &image, dev)
                    .with_context(|| format!("restaurando «{}»", p.title))?;
                done.push(format!("{}: idéntica bit a bit ({d})", p.title));
            }
            PartPlan::Btrfs {
                items,
                uuid,
                label,
                default_subvolume,
            } => {
                let n = restore_btrfs(s, &items, &uuid, label.as_deref(), default_subvolume, dev)
                    .with_context(|| format!("restaurando «{}»", p.title))?;
                done.push(format!(
                    "{}: {n} subvolumen(es) restaurados, UUID {uuid}",
                    p.title
                ));
            }
        }
    }
    let _ = s.run_unchecked(&Cmd::new("sync"));
    Ok(done)
}

/// Lee el informe de una parte y comprueba el hash de cada archivo.
fn load_part(report: &Path, part: &SetPart) -> Result<PartPlan> {
    if let Some((stream, image)) = raw::load_from_report(report)? {
        check_stream(&stream)?;
        return Ok(PartPlan::Raw(Box::new((stream, image))));
    }
    let v = load_report_value(report)?;
    let mut items: Vec<SubvolArtifact> = serde_json::from_value(v["subvolumes"].clone())
        .with_context(|| format!("{} no tiene subvolúmenes", report.display()))?;
    if items.is_empty() {
        bail!("{} no tiene subvolúmenes", report.display());
    }
    let dir = report.parent().unwrap_or(Path::new("."));
    for link in items.iter_mut().flat_map(|i| i.chain.iter_mut()) {
        if !link.path.exists() {
            if let Some(f) = link.path.file_name() {
                link.path = dir.join(f);
            }
        }
        check_stream(link)?;
    }
    let uuid = v["superblock"]["fsid"]
        .as_str()
        .ok_or_else(|| anyhow!("{} no guarda el UUID del Btrfs", report.display()))?
        .to_string();
    let label = v["superblock"]["label"]
        .as_str()
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    Ok(PartPlan::Btrfs {
        items,
        uuid,
        label,
        default_subvolume: part.default_subvolume.clone(),
    })
}

fn check_stream(stream: &partshield_core::types::StreamRef) -> Result<()> {
    if !stream.path.exists() {
        bail!("falta {}", stream.path.display());
    }
    if let Some(want) = &stream.digest {
        let got = Digest::of_file(want.algo, &stream.path)?;
        if got != *want {
            bail!(
                "{} está dañado: {got}, se esperaba {want}",
                stream.path.display()
            );
        }
    }
    Ok(())
}

/// Crea un Btrfs nuevo en `dev`, recibe cada subvolumen con su nombre
/// original y le pone el UUID del original, para que fstab y el gestor de
/// arranque sigan valiendo.
fn restore_btrfs(
    s: &mut Session,
    items: &[SubvolArtifact],
    uuid: &str,
    label: Option<&str>,
    default_subvolume: Option<String>,
    dev: &Path,
) -> Result<usize> {
    raw::ensure_not_mounted(s, dev)?;
    let mut mkfs = Cmd::new("mkfs.btrfs").arg("-q").arg("-f");
    if let Some(l) = label {
        mkfs = mkfs.arg("-L").arg(l);
    }
    s.run(mkfs.arg(dev))?;
    let name = dev
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "btrfs".into());
    let mnt = PathBuf::from(RUN_DIR).join("restaurar").join(name);
    std::fs::create_dir_all(&mnt)?;
    s.run(Cmd::new("mount").arg(dev).arg(&mnt))?;
    let result = (|| -> Result<usize> {
        let restored = restore_many(s, items, &mnt)?;
        // El subvolumen por defecto: el que tenía el original o, si el
        // Btrfs no tenía subvolúmenes, el único restaurado.
        let wanted = default_subvolume.map(|p| subvol_label(&p)).or_else(|| {
            (items.len() == 1 && items[0].label == subvol_label("")).then(|| items[0].label.clone())
        });
        for (label, r) in &restored {
            if wanted.as_deref() == Some(label.as_str()) {
                if let Some(w) = &r.writable {
                    s.run(Cmd::new("btrfs").arg("subvolume").arg("set-default").arg(w))?;
                }
            }
            // La instantánea recibida (solo lectura) ya no hace falta.
            if r.writable.is_some() {
                let _ = s.run_unchecked(
                    &Cmd::new("btrfs")
                        .arg("subvolume")
                        .arg("delete")
                        .arg(&r.received),
                );
            }
        }
        Ok(restored.len())
    })();
    let um = s.run(Cmd::new("umount").arg(&mnt));
    let n = result?;
    um?;
    apply_fs_uuid(s, uuid, dev)?;
    Ok(n)
}
