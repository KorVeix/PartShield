//! Copias restaurables y su restauración, sin GTK.
//!
//! Tres formas de copia de un disco:
//! - Un **conjunto** (`<disco>-<fecha>.set.json`), que el modo sencillo
//!   escribe junto a las copias: de qué disco eran, dónde está su tabla de
//!   particiones y qué informe corresponde a cada partición.
//! - Una copia por particiones **sin conjunto** (versiones anteriores): un
//!   `.sfdisk` y los informes de las particiones de ese disco. Se adopta
//!   como conjunto en memoria.
//! - Un disco entero **en un solo archivo** (`backup --mode raw /dev/disco`).
//!
//! Restaurar es una sola acción: la tabla y después cada partición, o la
//! imagen entera. Antes de escribir nada se comprueban los hashes de todos
//! los archivos y que la copia cabe en el destino.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{anyhow, bail, Context, Result};
use partshield_core::cmd::{Cmd, Session};
use partshield_core::detect::devices::{human_size, Disk};
use partshield_core::raw;
use partshield_core::report::load_report_value;
use partshield_core::send::layout::apply_fs_uuid;
use partshield_core::send::multi::restore_many;
use partshield_core::types::{Digest, RawImage, StreamRef, SubvolArtifact};
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
    /// `sfdisk --dump`. `None` si no era una partición de la tabla.
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

// ---------------------------------------------------- tabla de particiones

/// Un volcado `sfdisk --dump`, lo justo para saber si cabe en otro disco.
#[derive(Debug, Clone, PartialEq)]
pub struct DumpTable {
    pub gpt: bool,
    pub sector: u64,
    /// Líneas de cabecera (`label:`, `label-id:`…), sin `last-lba`.
    header: Vec<String>,
    pub parts: Vec<DumpPart>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DumpPart {
    pub node: String,
    pub start: u64,
    pub size: u64,
    /// Lo que va tras «nodo :», tal cual.
    spec: String,
}

/// Sectores que la GPT de respaldo ocupa al final del disco.
const GPT_TAIL: u64 = 34;

impl DumpTable {
    pub fn parse(dump: &str) -> Result<Self> {
        let mut t = DumpTable {
            gpt: false,
            sector: 512,
            header: Vec::new(),
            parts: Vec::new(),
        };
        for line in dump.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let field = |spec: &str, key: &str| -> Option<u64> {
                spec.split(',')
                    .filter_map(|f| f.split_once('='))
                    .find(|(k, _)| k.trim() == key)
                    .and_then(|(_, v)| v.trim().parse().ok())
            };
            match line.split_once(" : ") {
                Some((node, spec)) if spec.contains("start=") => t.parts.push(DumpPart {
                    node: node.trim().to_string(),
                    start: field(spec, "start")
                        .ok_or_else(|| anyhow!("partición sin inicio: {line}"))?,
                    size: field(spec, "size")
                        .ok_or_else(|| anyhow!("partición sin tamaño: {line}"))?,
                    spec: spec.trim().to_string(),
                }),
                _ => {
                    if let Some(v) = line.strip_prefix("label:") {
                        t.gpt = v.trim() == "gpt";
                    }
                    if let Some(v) = line.strip_prefix("sector-size:") {
                        t.sector = v.trim().parse().context("sector-size")?;
                    }
                    // El final lo recalcula sfdisk para el disco de destino.
                    if !line.starts_with("last-lba:") {
                        t.header.push(line.to_string());
                    }
                }
            }
        }
        if t.parts.is_empty() {
            bail!("la tabla de particiones no tiene particiones");
        }
        Ok(t)
    }

    fn tail(&self) -> u64 {
        if self.gpt {
            GPT_TAIL
        } else {
            0
        }
    }

    /// Índice de la partición que está físicamente al final.
    pub fn last(&self) -> usize {
        (0..self.parts.len())
            .max_by_key(|&i| self.parts[i].start)
            .unwrap_or(0)
    }

    /// Bytes del disco más pequeño donde cabe sin cambiar nada.
    pub fn bytes_as_is(&self) -> u64 {
        let end = self
            .parts
            .iter()
            .map(|p| p.start + p.size)
            .max()
            .unwrap_or(0);
        (end + self.tail()) * self.sector
    }

    /// Bytes del disco más pequeño donde cabe si la última partición es un
    /// Btrfs con `used` bytes de datos, que se rehace más pequeña.
    pub fn bytes_shrunk(&self, used: u64) -> u64 {
        let last = &self.parts[self.last()];
        (last.start + self.tail()) * self.sector + btrfs_room(used)
    }

    /// El volcado para sfdisk. Con `shrink_last`, la última partición no
    /// lleva tamaño y sfdisk le da todo lo que quede.
    pub fn render(&self, shrink_last: bool) -> String {
        let last = self.last();
        let mut out: String = self.header.iter().map(|l| format!("{l}\n")).collect();
        out.push('\n');
        for (i, p) in self.parts.iter().enumerate() {
            let spec = if shrink_last && i == last {
                p.spec
                    .split(',')
                    .filter(|f| f.split_once('=').map(|(k, _)| k.trim()) != Some("size"))
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                p.spec.clone()
            };
            out.push_str(&format!("{} : {}\n", p.node, spec.trim()));
        }
        out
    }
}

/// Sitio que necesita un Btrfs para recibir `used` bytes de datos: un
/// cuarto de margen para metadatos y 1 GiB más.
fn btrfs_room(used: u64) -> u64 {
    used + used / 4 + (1 << 30)
}

/// Cómo escribir la tabla en un disco de `target` bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    /// Tal cual: mismos inicios y tamaños.
    Keep,
    /// La última partición (un Btrfs) se crea más pequeña.
    ShrinkLast,
}

/// Decide si la tabla cabe en `target` bytes. `last_btrfs_used`: bytes de
/// datos del Btrfs de la última partición, si la última lo es.
pub fn plan_layout(table: &DumpTable, target: u64, last_btrfs_used: Option<u64>) -> Result<Layout> {
    if target >= table.bytes_as_is() {
        return Ok(Layout::Keep);
    }
    let Some(used) = last_btrfs_used else {
        bail!(
            "el disco de destino ({}) es más pequeño que el original y la última \
             partición no es un Btrfs: una imagen sector a sector no se puede encoger. \
             Hace falta un disco de al menos {}",
            human_size(target),
            human_size(table.bytes_as_is())
        );
    };
    let need = table.bytes_shrunk(used);
    if target < need {
        bail!(
            "el disco de destino ({}) es demasiado pequeño: con el Btrfs encogido a sus \
             datos ({}) hacen falta al menos {}",
            human_size(target),
            human_size(used),
            human_size(need)
        );
    }
    Ok(Layout::ShrinkLast)
}

// ----------------------------------------------------------------- buscar

/// De dónde sale una copia restaurable.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Un `.set.json`.
    SetFile,
    /// Un `.sfdisk` y los informes de sus particiones, sin conjunto.
    Adopted,
    /// Un disco entero en un solo archivo: su informe.
    Image,
}

/// Una copia de disco encontrada en un disco de copias.
#[derive(Debug, Clone)]
pub struct FoundSet {
    pub set: BackupSet,
    pub source: Source,
    /// Carpeta donde están el conjunto, la tabla y los informes.
    pub dir: PathBuf,
    /// Disco físico donde está la copia: nunca se ofrece como destino.
    pub on_disk: PathBuf,
    /// Bytes del disco más pequeño donde se puede restaurar.
    pub min_size: u64,
}

impl FoundSet {
    /// Por qué no se puede restaurar como disco entero, si no se puede.
    pub fn not_restorable(&self) -> Option<String> {
        if self.source == Source::Image {
            return None;
        }
        if self.set.table.is_none() {
            return Some("no se copió el disco entero: falta la tabla de particiones".into());
        }
        if let Some(p) = self.set.parts.iter().find(|p| p.report.is_none()) {
            return Some(format!("la copia de «{}» falló", p.title));
        }
        if let Some(p) = self.set.parts.iter().find(|p| p.index.is_none()) {
            return Some(format!("«{}» no es una partición de la tabla", p.title));
        }
        None
    }
}

/// Busca copias de disco en los discos que no son el USB de arranque. Los
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

/// Lo que se necesita de un informe para listar y adoptar copias.
struct ReportInfo {
    name: String,
    raw: bool,
    device: String,
    started: String,
    verdict: SetVerdict,
    error: Option<String>,
    size: u64,
    btrfs_used: Option<u64>,
}

fn report_info(path: &Path) -> Option<ReportInfo> {
    let v = load_report_value(path).ok()?;
    let error = v["error"].as_str().map(str::to_string);
    let verdict = match (v["verification"]["verdict"].as_str(), &error) {
        (Some("valid"), None) => SetVerdict::Valid,
        (Some("reduced"), None) => SetVerdict::Reduced,
        _ => SetVerdict::Invalid,
    };
    let raw = v["mode"].as_str() == Some("raw");
    Some(ReportInfo {
        name: path.file_name()?.to_string_lossy().into_owned(),
        raw,
        device: v["device"].as_str()?.to_string(),
        started: v["started"].as_str()?.to_string(),
        verdict,
        error,
        size: if raw {
            v["raw"]["size_bytes"].as_u64().unwrap_or(0)
        } else {
            v["superblock"]["total_bytes"].as_u64().unwrap_or(0)
        },
        btrfs_used: (!raw)
            .then(|| v["superblock"]["bytes_used"].as_u64())
            .flatten(),
    })
}

/// `nvme0n1-20261127-221500.sfdisk` → («nvme0n1», «2026-11-27T22:15:00»).
fn table_stamp(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".sfdisk")?;
    let mut it = stem.rsplitn(3, '-');
    let (time, date, disk) = (it.next()?, it.next()?, it.next()?);
    let t =
        chrono::NaiveDateTime::parse_from_str(&format!("{date}-{time}"), "%Y%m%d-%H%M%S").ok()?;
    Some((disk.to_string(), t.format("%Y-%m-%dT%H:%M:%S").to_string()))
}

pub(crate) fn sets_in(dir: &Path, on_disk: &Path) -> Vec<FoundSet> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    let named = |suffix: &str| -> Vec<&PathBuf> {
        files
            .iter()
            .filter(|p| p.to_string_lossy().ends_with(suffix))
            .collect()
    };
    let reports: Vec<ReportInfo> = named(".report.json")
        .into_iter()
        .filter_map(|p| report_info(p))
        .collect();
    let found = |set: BackupSet, source: Source, min_size: u64| FoundSet {
        set,
        source,
        dir: dir.to_path_buf(),
        on_disk: on_disk.to_path_buf(),
        min_size,
    };
    let mut out = Vec::new();
    let mut used_tables = Vec::new();
    let mut used_reports: Vec<String> = Vec::new();

    // 1. Conjuntos.
    for path in named(SET_SUFFIX) {
        let Ok(set) = BackupSet::load(path) else {
            continue;
        };
        used_tables.extend(set.table.clone());
        used_reports.extend(set.parts.iter().filter_map(|p| p.report.clone()));
        let min = min_size_of(dir, &set, &reports).unwrap_or(set.disk.size);
        out.push(found(set, Source::SetFile, min));
    }

    // 2. Tablas sin conjunto: se adoptan con los informes de sus particiones.
    let mut tables: Vec<(String, String, String)> = named(".sfdisk")
        .into_iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !used_tables.contains(n))
        .filter_map(|n| table_stamp(&n).map(|(disk, at)| (n, disk, at)))
        .collect();
    tables.sort_by(|a, b| a.2.cmp(&b.2));
    for (i, (file, disk, at)) in tables.iter().enumerate() {
        // Hasta la siguiente copia del mismo disco.
        let until = tables[i + 1..]
            .iter()
            .find(|t| t.1 == *disk)
            .map(|t| t.2.as_str());
        let Some(set) = adopt(dir, file, at, until, &reports, &used_reports) else {
            continue;
        };
        used_reports.extend(set.parts.iter().filter_map(|p| p.report.clone()));
        let min = min_size_of(dir, &set, &reports).unwrap_or(set.disk.size);
        out.push(found(set, Source::Adopted, min));
    }

    // 3. Discos enteros en un solo archivo.
    for r in reports
        .iter()
        .filter(|r| r.raw && !used_reports.contains(&r.name))
    {
        let Ok(Some((stream, image))) = raw::load_from_report(&dir.join(&r.name)) else {
            continue;
        };
        if !starts_with_gpt(&stream.path) {
            continue;
        }
        let set = BackupSet {
            version: 1,
            created: r.started.clone(),
            disk: SetDisk {
                path: r.device.clone(),
                model: None,
                serial: None,
                size: image.size_bytes,
            },
            table: None,
            parts: vec![SetPart {
                index: None,
                device: r.device.clone(),
                title: format!(
                    "Disco entero en un solo archivo · {}",
                    human_size(image.size_bytes)
                ),
                size: image.size_bytes,
                fstype: None,
                report: Some(r.name.clone()),
                verdict: r.verdict,
                error: r.error.clone(),
                default_subvolume: None,
            }],
        };
        out.push(found(set, Source::Image, image.size_bytes));
    }
    out
}

/// Conjunto en memoria para una tabla sin `.set.json`: de cada partición
/// del volcado, su informe más reciente hecho entre `at` y `until`.
fn adopt(
    dir: &Path,
    table_file: &str,
    at: &str,
    until: Option<&str>,
    reports: &[ReportInfo],
    taken: &[String],
) -> Option<BackupSet> {
    let dump = std::fs::read_to_string(dir.join(table_file)).ok()?;
    let table = DumpTable::parse(&dump).ok()?;
    let disk_path = dump
        .lines()
        .find_map(|l| l.strip_prefix("device:"))
        .map(|d| d.trim().to_string())
        .unwrap_or_default();
    let mut parts = Vec::new();
    for (index, p) in table.parts.iter().enumerate() {
        let best = reports
            .iter()
            .filter(|r| r.device == p.node && !taken.contains(&r.name))
            .filter(|r| r.started.as_str() >= at && until.map_or(true, |u| r.started.as_str() < u))
            .max_by(|a, b| a.started.cmp(&b.started));
        let Some(r) = best else { continue };
        let name = Path::new(&p.node)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        parts.push(SetPart {
            index: Some(index),
            device: p.node.clone(),
            title: format!(
                "{name} ({}) · {}",
                if r.raw { "sector a sector" } else { "Btrfs" },
                human_size(p.size * table.sector)
            ),
            size: r.size,
            fstype: (!r.raw).then(|| "btrfs".to_string()),
            report: Some(r.name.clone()),
            verdict: r.verdict,
            error: r.error.clone(),
            default_subvolume: None,
        });
    }
    if parts.is_empty() {
        return None;
    }
    Some(BackupSet {
        version: 1,
        created: at.to_string(),
        disk: SetDisk {
            path: disk_path,
            model: None,
            serial: None,
            size: table.bytes_as_is(),
        },
        table: Some(table_file.to_string()),
        parts,
    })
}

/// El disco más pequeño donde cabe el conjunto: su tabla tal cual, o con
/// la última partición encogida si es un Btrfs copiado en el conjunto.
fn min_size_of(dir: &Path, set: &BackupSet, reports: &[ReportInfo]) -> Option<u64> {
    let dump = std::fs::read_to_string(dir.join(set.table.as_deref()?)).ok()?;
    let table = DumpTable::parse(&dump).ok()?;
    let used = last_btrfs_used(&table, set, |name| {
        reports
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| r.btrfs_used)
    });
    Some(match used {
        Some(u) => table.bytes_shrunk(u).min(table.bytes_as_is()),
        None => table.bytes_as_is(),
    })
}

/// Bytes de datos del Btrfs de la última partición de la tabla, si el
/// conjunto la copió como Btrfs.
fn last_btrfs_used(
    table: &DumpTable,
    set: &BackupSet,
    used_of: impl Fn(&str) -> Option<u64>,
) -> Option<u64> {
    let last = table.last();
    set.parts
        .iter()
        .find(|p| p.index == Some(last))
        .and_then(|p| p.report.as_deref())
        .and_then(used_of)
}

/// La imagen comprimida empieza por una tabla GPT («EFI PART» en el
/// segundo sector, de 512 o de 4096 bytes).
fn starts_with_gpt(stream: &Path) -> bool {
    let Ok(f) = std::fs::File::open(stream) else {
        return false;
    };
    let Ok(mut dec) = zstd::Decoder::new(f) else {
        return false;
    };
    let mut head = vec![0u8; 8192];
    let mut n = 0;
    while n < head.len() {
        match dec.read(&mut head[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    [512usize, 4096]
        .iter()
        .any(|&o| head.get(o..o + 8) == Some(b"EFI PART".as_slice()))
}

/// Discos donde se puede restaurar `found`: ni el USB de arranque ni el
/// disco que guarda la copia, y donde quepa.
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
        .filter(|d| d.size >= found.min_size)
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

/// Pasos de una restauración, para la barra: la tabla y cada partición, o
/// la imagen entera.
pub fn restore_steps(found: &FoundSet) -> usize {
    match found.source {
        Source::Image => 1,
        _ => 1 + found.set.parts.len(),
    }
}

/// Restaura `found` en `target`, que se borra entero.
pub fn run_restore(found: &FoundSet, target: &Path, events: Sender<RestoreEvent>) {
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
    let r = match found.source {
        Source::Image => restore_image(&mut s, found, target, &events),
        _ => restore_set(&mut s, found, target, &events),
    };
    drop(s);
    let _ = fwd.join();
    let _ = events.send(RestoreEvent::Finished(r.map_err(|e| format!("{e:#}"))));
}

fn target_size(s: &mut Session, target: &Path) -> Result<u64> {
    partshield_core::cmd::guard_path(target)?;
    raw::ensure_not_mounted(s, target)?;
    s.run(Cmd::new("blockdev").arg("--getsize64").arg(target))?
        .trim()
        .parse()
        .context("tamaño del disco de destino")
}

/// Disco entero en un solo archivo: la imagen tal cual y, en un disco
/// mayor, la GPT de respaldo llevada al final.
fn restore_image(
    s: &mut Session,
    found: &FoundSet,
    target: &Path,
    events: &Sender<RestoreEvent>,
) -> Result<Vec<String>> {
    let part = found
        .set
        .parts
        .first()
        .ok_or_else(|| anyhow!("copia vacía"))?;
    let report = found.dir.join(part.report.as_deref().unwrap_or_default());
    let (stream, image) = raw::load_from_report(&report)?
        .ok_or_else(|| anyhow!("{} no es una imagen de disco", report.display()))?;
    check_stream(&stream).context("no se ha escrito nada en el disco")?;
    let size = target_size(s, target)?;
    if size < image.size_bytes {
        bail!(
            "{} mide {} y la imagen {}: un disco entero en un solo archivo no cabe en \
             un disco más pequeño. No se ha escrito nada",
            target.display(),
            human_size(size),
            human_size(image.size_bytes)
        );
    }
    let _ = events.send(RestoreEvent::Step(0, part.title.clone()));
    let d = raw::restore_to(s, &stream, &image, target)?;
    let mut done = vec![format!(
        "Imagen restaurada en {}: idéntica bit a bit ({d})",
        target.display()
    )];
    if size > image.size_bytes {
        s.run(Cmd::new("sgdisk").arg("-e").arg(target))
            .context("llevando la tabla GPT de respaldo al final del disco")?;
        done.push("Tabla GPT de respaldo llevada al final del disco, que es mayor".into());
    }
    let _ = s.run_unchecked(&Cmd::new("partx").arg("-u").arg(target));
    let _ = s.run_unchecked(&Cmd::new("sync"));
    Ok(done)
}

/// Qué hay que escribir en una partición.
enum PartPlan {
    Raw(Box<(StreamRef, RawImage)>),
    Btrfs {
        items: Vec<SubvolArtifact>,
        uuid: String,
        label: Option<String>,
        used: u64,
        default_subvolume: Option<String>,
    },
}

fn restore_set(
    s: &mut Session,
    found: &FoundSet,
    target: &Path,
    events: &Sender<RestoreEvent>,
) -> Result<Vec<String>> {
    let set = &found.set;
    if let Some(why) = found.not_restorable() {
        bail!("esta copia no se puede restaurar como disco entero: {why}");
    }
    let dir = &found.dir;
    let table_path = dir.join(set.table.as_deref().unwrap_or_default());
    let dump = std::fs::read_to_string(&table_path)
        .with_context(|| format!("leyendo la tabla de particiones {}", table_path.display()))?;
    let table = DumpTable::parse(&dump)?;

    // Antes de escribir nada: todos los informes se leen, todos los
    // archivos de la copia tienen el hash esperado y la copia cabe.
    let mut plans = Vec::new();
    for p in &set.parts {
        let report = dir.join(p.report.as_deref().unwrap_or_default());
        let plan = load_part(&report, p)
            .with_context(|| format!("«{}»: no se ha escrito nada en el disco", p.title))?;
        plans.push(plan);
    }
    let size = target_size(s, target)?;
    let sector: u64 = s
        .run(Cmd::new("blockdev").arg("--getss").arg(target))?
        .trim()
        .parse()
        .context("tamaño de sector del disco de destino")?;
    if sector != table.sector {
        bail!(
            "{} tiene sectores de {sector} bytes y el disco original de {}: la tabla no \
             vale. No se ha escrito nada",
            target.display(),
            table.sector
        );
    }
    let last = table.last();
    let last_used = set
        .parts
        .iter()
        .zip(&plans)
        .find(|(p, _)| p.index == Some(last))
        .and_then(|(_, plan)| match plan {
            PartPlan::Btrfs { used, .. } => Some(*used),
            PartPlan::Raw(_) => None,
        });
    let layout =
        plan_layout(&table, size, last_used).context("no se ha escrito nada en el disco")?;

    // 1. Tabla de particiones.
    let _ = events.send(RestoreEvent::Step(0, "Tabla de particiones".into()));
    let text = table.render(layout == Layout::ShrinkLast);
    s.pipe_in(
        Cmd::new("sfdisk").arg("--wipe").arg("always").arg(target),
        |stdin| Ok(stdin.write_all(text.as_bytes())?),
    )
    .context("escribiendo la tabla de particiones")?;
    let _ = s.run_unchecked(&Cmd::new("partx").arg("-u").arg(target));
    let _ = s.run_unchecked(&Cmd::new("udevadm").arg("settle"));
    let mut done = vec![match layout {
        Layout::Keep => format!("Tabla de particiones escrita en {}", target.display()),
        Layout::ShrinkLast => format!(
            "Tabla de particiones escrita en {}, con la última partición más pequeña \
             porque el disco es menor",
            target.display()
        ),
    }];

    // 2. Cada partición.
    let devices = PartDevices::open(s, target)?;
    let result = (|| -> Result<()> {
        for (i, (p, plan)) in set.parts.iter().zip(plans).enumerate() {
            let _ = events.send(RestoreEvent::Step(i + 1, p.title.clone()));
            let dev = p
                .index
                .and_then(|ix| devices.nodes.get(ix))
                .ok_or_else(|| {
                    anyhow!("«{}»: el disco de destino no tiene esa partición", p.title)
                })?;
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
                    ..
                } => {
                    let n =
                        restore_btrfs(s, &items, &uuid, label.as_deref(), default_subvolume, dev)
                            .with_context(|| format!("restaurando «{}»", p.title))?;
                    done.push(format!(
                        "{}: {n} subvolumen(es) restaurados, UUID {uuid}",
                        p.title
                    ));
                }
            }
        }
        Ok(())
    })();
    devices.close(s);
    result?;
    let _ = s.run_unchecked(&Cmd::new("sync"));
    Ok(done)
}

/// Los dispositivos de las particiones recién creadas en `disk`. Si el
/// kernel no crea sus nodos (sin `partscan`, como en algunos contenedores y
/// máquinas virtuales), cada partición se enlaza a un loop con `--offset`.
struct PartDevices {
    nodes: Vec<PathBuf>,
    loops: Vec<PathBuf>,
}

impl PartDevices {
    fn open(s: &mut Session, disk: &Path) -> Result<Self> {
        let json = s.run(Cmd::new("sfdisk").arg("--json").arg(disk))?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        let sector = v["partitiontable"]["sectorsize"].as_u64().unwrap_or(512);
        let parts: Vec<(PathBuf, u64, u64)> = v["partitiontable"]["partitions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some((
                    PathBuf::from(p["node"].as_str()?),
                    p["start"].as_u64()? * sector,
                    p["size"].as_u64()? * sector,
                ))
            })
            .collect();
        for _ in 0..30 {
            if parts.iter().all(|(n, _, _)| n.exists()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if parts.iter().all(|(n, _, _)| n.exists()) {
            return Ok(Self {
                nodes: parts.into_iter().map(|(n, _, _)| n).collect(),
                loops: Vec::new(),
            });
        }
        let mut out = Self {
            nodes: Vec::new(),
            loops: Vec::new(),
        };
        for (_, start, size) in parts {
            let dev = s.run(
                Cmd::new("losetup")
                    .arg("--find")
                    .arg("--show")
                    .arg("--offset")
                    .arg(start.to_string())
                    .arg("--sizelimit")
                    .arg(size.to_string())
                    .arg(disk),
            );
            match dev {
                Ok(d) => {
                    let d = PathBuf::from(d.trim());
                    out.loops.push(d.clone());
                    out.nodes.push(d);
                }
                Err(e) => {
                    out.close(s);
                    return Err(e.context("enlazando las particiones del disco de destino"));
                }
            }
        }
        Ok(out)
    }

    fn close(&self, s: &mut Session) {
        for l in &self.loops {
            let _ = s.run_unchecked(&Cmd::new("losetup").arg("--detach").arg(l));
        }
    }
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
        used: v["superblock"]["bytes_used"]
            .as_u64()
            .unwrap_or(u64::MAX / 4),
        default_subvolume: part.default_subvolume.clone(),
    })
}

fn check_stream(stream: &StreamRef) -> Result<()> {
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
    // El kernel recuerda los Btrfs que ha visto aunque su disco ya no esté
    // (el original, si se copió en esta misma sesión): que los olvide, o
    // btrfstune dirá que el UUID no es único. Los que siguen conectados no
    // se olvidan, y entonces el error es cierto.
    let _ = s.run_unchecked(&Cmd::new("btrfs").arg("device").arg("scan").arg("--forget"));
    apply_fs_uuid(s, uuid, dev)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "label: gpt\n\
        label-id: 11111111-2222-3333-4444-555555555555\n\
        device: /dev/nvme0n1\n\
        unit: sectors\n\
        first-lba: 2048\n\
        last-lba: 1228766\n\
        sector-size: 512\n\
        \n\
        /dev/nvme0n1p1 : start=        2048, size=      131072, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=AAAAAAAA-0000-0000-0000-000000000001\n\
        /dev/nvme0n1p2 : start=      133120, size=     1095647, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, uuid=AAAAAAAA-0000-0000-0000-000000000002\n";

    #[test]
    fn dump_keeps_everything_but_the_last_lba() {
        let t = DumpTable::parse(DUMP).unwrap();
        assert!(t.gpt);
        assert_eq!(t.sector, 512);
        assert_eq!(t.parts.len(), 2);
        assert_eq!(t.last(), 1);
        assert_eq!(t.bytes_as_is(), (133120 + 1095647 + 34) * 512);
        let same = t.render(false);
        assert!(!same.contains("last-lba"));
        assert!(same.contains("label-id: 11111111"));
        assert!(same.contains("/dev/nvme0n1p2 : start=      133120, size=     1095647, type="));
        let shrunk = t.render(true);
        assert!(shrunk.contains("/dev/nvme0n1p1 : start=        2048, size=      131072"));
        let last = shrunk.lines().last().unwrap();
        assert!(
            last.starts_with("/dev/nvme0n1p2 : start=      133120, type="),
            "{last}"
        );
        assert!(last.contains("uuid=AAAAAAAA-0000-0000-0000-000000000002"));
    }

    #[test]
    fn layout_for_bigger_equal_and_smaller_disks() {
        let t = DumpTable::parse(DUMP).unwrap();
        let as_is = t.bytes_as_is();
        let gib = 1u64 << 30;
        assert_eq!(plan_layout(&t, as_is, None).unwrap(), Layout::Keep);
        assert_eq!(plan_layout(&t, as_is * 4, None).unwrap(), Layout::Keep);
        // Más pequeño y la última no es Btrfs: no se puede.
        let e = plan_layout(&t, as_is - 512, None).unwrap_err().to_string();
        assert!(e.contains("no es un Btrfs"), "{e}");
        // Más pequeño, con un Btrfs al final cuyos datos caben.
        let t2 = DumpTable::parse(&DUMP.replace("1095647", "104857600")).unwrap();
        let small = 10 * gib;
        assert_eq!(
            plan_layout(&t2, small, Some(2 * gib)).unwrap(),
            Layout::ShrinkLast
        );
        // Sus datos no caben.
        let e = plan_layout(&t2, small, Some(20 * gib))
            .unwrap_err()
            .to_string();
        assert!(e.contains("demasiado pequeño"), "{e}");
    }

    #[test]
    fn table_file_names_give_disk_and_time() {
        assert_eq!(
            table_stamp("nvme0n1-20261127-221500.sfdisk"),
            Some(("nvme0n1".into(), "2026-11-27T22:15:00".into()))
        );
        assert_eq!(table_stamp("otra-cosa.sfdisk"), None);
    }

    #[test]
    fn confirmation_is_the_end_of_the_serial() {
        let mut d = Disk {
            path: PathBuf::from("/dev/sdb"),
            kind: "disk".into(),
            size: 1,
            model: None,
            serial: Some("BTPY74520RBN256D".into()),
            transport: None,
            removable: false,
            volumes: vec![],
        };
        assert_eq!(confirmation_word(&d), "256D");
        d.serial = None;
        assert_eq!(confirmation_word(&d), "BORRAR");
    }
}
