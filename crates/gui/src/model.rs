//! Lo que la interfaz pide al motor, sin GTK: se puede probar sin pantalla.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{anyhow, bail, Context, Result};
use partshield_core::cmd::{Cmd, CommandRecord, Session};
use partshield_core::detect::devices::{self, Disk, Volume};
use partshield_core::detect::modes::{assess_with_kernel, KernelFeatures};
use partshield_core::detect::superblock::SuperblockCopies;
use partshield_core::detect::SuperblockExt;
use partshield_core::detect::{block_stack, read_all_copies};
use partshield_core::send::multi::SubvolSpec;
use partshield_core::types::{Assessment, BlockStack, HashAlgo, Mode, Report, Superblock, Verdict};

use crate::restore::{BackupSet, SetDisk, SetPart, SetVerdict};
use partshield_core::{backup, backup_raw, partclone_version, BackupOptions, RawOptions};

pub fn list_disks() -> Result<(Vec<Disk>, Vec<CommandRecord>)> {
    // Para capturas: discos de un JSON de `lsblk -J -b` en vez de los reales.
    if let Some(p) = std::env::var_os("PARTSHIELD_GUI_LSBLK") {
        return Ok((devices::parse(&std::fs::read_to_string(p)?)?, Vec::new()));
    }
    let mut s = Session::new(false);
    let disks = devices::list(&mut s)?;
    Ok((disks, s.records))
}

/// Todo lo que se sabe de un Btrfs sin escribir nada.
pub struct DetectInfo {
    pub device: PathBuf,
    pub copies: SuperblockCopies,
    pub superblock: Superblock,
    pub modes: Vec<Assessment>,
    pub stack: Result<BlockStack, String>,
    pub warnings: Vec<String>,
    pub commands: Vec<CommandRecord>,
}

pub fn detect(device: &Path) -> Result<DetectInfo> {
    let mut s = Session::new(false);
    let copies = read_all_copies(device).map_err(|e| {
        let hint = if e.to_string().contains("Permission denied") {
            " (hace falta ejecutar PartShield como root)"
        } else {
            ""
        };
        anyhow!("{e}{hint}")
    })?;
    let superblock = copies.primary().cloned().ok_or_else(|| {
        anyhow!(
            "el superbloque principal de {} no es válido",
            device.display()
        )
    })?;
    let modes = assess_with_kernel(
        &superblock.features,
        partclone_version(&mut s),
        KernelFeatures::from_sysfs().as_ref(),
    );
    let stack = block_stack(&mut s, device).map_err(|e| format!("{e:#}"));
    let warnings = copies.discrepancies();
    Ok(DetectInfo {
        device: device.to_path_buf(),
        copies,
        superblock,
        modes,
        stack,
        warnings,
        commands: s.records,
    })
}

// ------------------------------------------------------------------ copia

/// Dónde monta PartShield lo que necesita, fuera de la vista del usuario.
pub(crate) const RUN_DIR: &str = "/run/partshield";

/// Un subvolumen de `btrfs subvolume list`.
#[derive(Debug, Clone, PartialEq)]
pub struct Subvol {
    pub id: u64,
    pub top_level: u64,
    pub path: String,
}

impl Subvol {
    /// Snapshots de snapper, timeshift y los que deja PartShield: no se
    /// copian por defecto.
    pub fn looks_like_snapshot(&self) -> bool {
        self.path.split('/').any(|c| {
            matches!(
                c,
                ".snapshots" | "@snapshots" | "@.snapshots" | "timeshift-btrfs"
            ) || c.starts_with(".partshield")
        }) || self.path.contains("--partshield-")
    }

    /// Lo que se marca solo: subvolúmenes de primer nivel que no son snapshots.
    pub fn selected_by_default(&self) -> bool {
        self.top_level == 5 && !self.looks_like_snapshot()
    }
}

/// `ID 256 gen 7 top level 5 path @home` → Subvol.
pub fn parse_subvolume_list(out: &str) -> Vec<Subvol> {
    out.lines()
        .filter_map(|l| {
            let id = l
                .strip_prefix("ID ")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            let (_, rest) = l.split_once(" top level ")?;
            let (top, path) = rest.split_once(" path ")?;
            Some(Subvol {
                id,
                top_level: top.trim().parse().ok()?,
                path: path.trim().to_string(),
            })
        })
        .collect()
}

/// El Btrfs montado entero (subvolid=5) para ver y copiar sus subvolúmenes.
pub struct SourceMount {
    pub mountpoint: PathBuf,
    pub subvolumes: Vec<Subvol>,
}

/// Monta el nivel superior en solo lectura bajo /run/partshield/origen.
/// La copia lo vuelve a montar en escritura justo antes de crear los
/// snapshots (`btrfs send` los necesita de solo lectura, pero crearlos escribe).
pub fn mount_source(device: &Path, fsid: &str) -> Result<SourceMount> {
    let mut s = Session::new(false);
    let mnt = PathBuf::from(RUN_DIR).join("origen").join(fsid);
    std::fs::create_dir_all(&mnt)?;
    if !is_mountpoint(&mut s, &mnt) {
        s.run(
            Cmd::new("mount")
                .arg("-t")
                .arg("btrfs")
                .arg("-o")
                .arg("ro,subvolid=5")
                .arg(device)
                .arg(&mnt),
        )
        .context("no se pudo montar el Btrfs de origen")?;
    }
    let out = s.run(Cmd::new("btrfs").arg("subvolume").arg("list").arg(&mnt))?;
    Ok(SourceMount {
        mountpoint: mnt,
        subvolumes: parse_subvolume_list(&out),
    })
}

pub(crate) fn is_mountpoint(s: &mut Session, path: &Path) -> bool {
    s.run_unchecked(&Cmd::new("mountpoint").arg("-q").arg(path))
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Un sitio donde guardar la copia.
#[derive(Debug, Clone)]
pub struct Destination {
    pub device: PathBuf,
    pub disk: String,
    pub fstype: String,
    pub label: Option<String>,
    pub size: u64,
    /// Si ya está montado en escritura, dónde.
    pub mountpoint: Option<PathBuf>,
}

impl Destination {
    pub fn title(&self) -> String {
        let name = self
            .device
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match &self.label {
            Some(l) => format!("{name} «{l}»"),
            None => name,
        }
    }
}

/// Sistemas de archivos en los que se puede escribir una copia.
pub(crate) const DEST_FSTYPES: [&str; 7] =
    ["btrfs", "ext4", "ext3", "xfs", "exfat", "vfat", "ntfs"];

/// Volúmenes de otros discos donde guardar la copia: nunca un disco de
/// origen ni el USB de arranque.
pub fn destinations(disks: &[Disk], sources: &[PathBuf]) -> Vec<Destination> {
    let source_disks: Vec<&PathBuf> = disks
        .iter()
        .filter(|d| {
            sources
                .iter()
                .any(|s| *s == d.path || d.volumes.iter().any(|v| v.path == *s))
        })
        .map(|d| &d.path)
        .collect();
    let mut out = Vec::new();
    for d in disks {
        if source_disks.contains(&&d.path) || d.volumes.iter().any(|v| v.is_live_medium()) {
            continue;
        }
        let disk = format!(
            "{} · {}",
            d.model.as_deref().unwrap_or(&d.path.display().to_string()),
            devices::human_size(d.size)
        );
        for v in &d.volumes {
            let Some(fstype) = v.fstype.as_deref().filter(|f| DEST_FSTYPES.contains(f)) else {
                continue;
            };
            out.push(Destination {
                device: v.path.clone(),
                disk: disk.clone(),
                fstype: fstype.to_string(),
                label: v.label.clone(),
                size: v.size,
                mountpoint: v
                    .mountpoints
                    .iter()
                    .find(|m| m.starts_with('/'))
                    .map(PathBuf::from),
            });
        }
    }
    out
}

/// Monta el destino si hace falta y devuelve la carpeta de copias.
pub fn prepare_destination(dest: &Destination, s: &mut Session) -> Result<PathBuf> {
    let root = match &dest.mountpoint {
        Some(m) => m.clone(),
        None => {
            let name = dest
                .device
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "destino".into());
            let mnt = PathBuf::from(RUN_DIR).join("destino").join(name);
            std::fs::create_dir_all(&mnt)?;
            if !is_mountpoint(s, &mnt) {
                s.run(Cmd::new("mount").arg(&dest.device).arg(&mnt))
                    .context("no se pudo montar el destino")?;
            }
            mnt
        }
    };
    Ok(root.join(crate::restore::COPIES_DIR))
}

/// Bytes libres en `path` (o en el primer antecesor que exista).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let mut s = Session::new(false);
    let out = s
        .run(Cmd::new("df").arg("--output=avail").arg("-B1").arg(p))
        .ok()?;
    out.lines().nth(1)?.trim().parse().ok()
}

/// Qué copiar.
#[derive(Debug, Clone)]
pub enum Job {
    /// btrfs send de los subvolúmenes elegidos (rutas relativas al nivel
    /// superior; "" es el propio nivel superior).
    Send {
        mount: PathBuf,
        subvolumes: Vec<String>,
        layout: bool,
        keep_snapshot: bool,
    },
    /// Imagen sector a sector del dispositivo entero.
    Raw { device: PathBuf },
}

pub struct JobOptions {
    pub dest: Destination,
    pub zstd_level: i32,
    pub hash: HashAlgo,
}

/// Etiqueta estable de un subvolumen: su ruta, con `/` cambiado por `_`.
pub fn subvol_label(path: &str) -> String {
    if path.is_empty() {
        "nivel-superior".into()
    } else {
        path.replace('/', "_")
    }
}

/// Lanza la copia con su verificación. Bloquea: llámalo fuera del hilo de
/// la interfaz. Cada comando se manda por `progress` antes de ejecutarse.
pub fn run_job(
    job: &Job,
    opts: &JobOptions,
    progress: Sender<String>,
) -> Result<(Report, PathBuf)> {
    let mut s = Session::new(false);
    s.progress = Some(progress);
    // Los montajes de preparación se quedan en la sesión: `backup` y
    // `backup_raw` los guardan en el informe junto con el resto.
    let dest_dir = prepare_destination(&opts.dest, &mut s)?;
    std::fs::create_dir_all(&dest_dir)?;
    let (report, path) = match job {
        Job::Raw { device } => backup_raw(
            &mut s,
            &RawOptions {
                device,
                dest_dir: &dest_dir,
                scratch_dir: None,
                zstd_level: opts.zstd_level,
                hash: opts.hash,
                sign_key: None,
            },
        )?,
        Job::Send {
            mount,
            subvolumes,
            layout,
            keep_snapshot,
        } => {
            if subvolumes.is_empty() {
                bail!("no hay ningún subvolumen elegido");
            }
            s.run(Cmd::new("mount").arg("-o").arg("remount,rw").arg(mount))
                .context("no se pudo montar el origen en escritura para crear los snapshots")?;
            let labels: Vec<String> = subvolumes.iter().map(|p| subvol_label(p)).collect();
            let sources: Vec<PathBuf> = subvolumes.iter().map(|p| mount.join(p)).collect();
            let specs: Vec<SubvolSpec> = labels
                .iter()
                .zip(&sources)
                .map(|(label, source)| SubvolSpec { label, source })
                .collect();
            let result = backup(
                &mut s,
                &BackupOptions {
                    source: mount,
                    dest_dir: &dest_dir,
                    snapshot_dir: Some(mount),
                    scratch_dir: None,
                    keep_snapshot: *keep_snapshot,
                    zstd_level: opts.zstd_level,
                    parent_report: None,
                    hash: opts.hash,
                    sign_key: None,
                    subvolumes: &specs,
                    capture_layout: *layout,
                    disk: None,
                },
            );
            // Vuelve a solo lectura pase lo que pase. El informe ya está
            // escrito (nunca se sobrescribe), así que un fallo aquí solo puede
            // ir al informe que muestra la interfaz.
            let ro = s.run_unchecked(&Cmd::new("mount").arg("-o").arg("remount,ro").arg(mount));
            let (mut report, path) = result?;
            report.commands.append(&mut s.records);
            if !ro.as_ref().is_ok_and(|o| o.success) {
                let why = match ro {
                    Ok(o) => o.stderr.trim().to_string(),
                    Err(e) => format!("{e:#}"),
                };
                report.warnings.push(format!(
                    "el origen sigue montado en escritura en {}: no se pudo volver a solo \
                     lectura ({why}). Desmóntalo (umount {}) antes de seguir; este aviso \
                     no está en el informe guardado, que se escribió antes",
                    mount.display(),
                    mount.display()
                ));
            }
            (report, path)
        }
    };
    Ok((report, path))
}

// ------------------------------------------------------------ modo sencillo

/// Nombre para personas: «Disco interno NVMe · Samsung 1 TB».
pub fn disk_title(d: &Disk) -> String {
    let kind = match (d.kind.as_str(), d.transport.as_deref()) {
        ("loop", _) => "Disco de pruebas",
        (_, Some("usb")) => "Disco USB",
        (_, Some("nvme")) => "Disco interno NVMe",
        _ if d.removable => "Disco extraíble",
        _ => "Disco interno",
    };
    match &d.model {
        Some(m) => format!("{kind} · {} · {}", m.trim(), devices::human_size(d.size)),
        None => format!("{kind} · {}", devices::human_size(d.size)),
    }
}

/// Qué es una partición, dicho para personas: «Windows (NTFS) «Datos»».
pub fn volume_title(v: &Volume) -> String {
    let small = v.size < (2 << 30);
    let what = match v.fstype.as_deref() {
        _ if v.is_msr() => "Reservada de Microsoft (MSR)",
        Some("BitLocker") => "Windows cifrado (BitLocker)",
        Some("ntfs") if small => "Recuperación de Windows",
        Some("ntfs") => "Windows (NTFS)",
        Some("vfat") if small => "Arranque (EFI)",
        Some("vfat" | "exfat") => "Datos (FAT)",
        Some("btrfs") => "Linux (Btrfs)",
        Some(f @ ("ext4" | "ext3" | "ext2" | "xfs")) => {
            return with_label(format!("Linux ({f})"), v);
        }
        Some("swap") => "Memoria de intercambio (swap)",
        Some("crypto_LUKS") => "Cifrada (LUKS)",
        Some(f) => return with_label(f.to_string(), v),
        None => "Sin formato",
    };
    with_label(what.to_string(), v)
}

fn with_label(s: String, v: &Volume) -> String {
    match &v.label {
        Some(l) if !l.is_empty() => format!("{s} «{l}»"),
        _ => s,
    }
}

/// Lo que se puede marcar en el modo sencillo: discos con sus particiones
/// de primer nivel. Nunca el USB de arranque; los loops solo en pruebas.
pub fn basic_disks(disks: &[Disk], show_loops: bool) -> Vec<&Disk> {
    disks
        .iter()
        .filter(|d| {
            !d.volumes
                .iter()
                .any(|v| v.is_live_medium() || v.is_live_system())
        })
        .filter(|d| show_loops || d.kind != "loop")
        .filter(|d| d.volumes.iter().any(|v| v.depth == 0))
        .collect()
}

/// El disco que se marca entero al abrir: el primer disco interno.
pub fn default_disk(disks: &[&Disk]) -> Option<PathBuf> {
    disks
        .iter()
        .find(|d| !d.removable && d.transport.as_deref() != Some("usb"))
        .map(|d| d.path.clone())
}

/// Marcada al marcar el disco entero. La swap y la reservada de Microsoft
/// no: no guardan datos y se rehacen solas.
pub fn checked_with_disk(v: &Volume) -> bool {
    v.fstype.as_deref() != Some("swap") && !v.is_msr()
}

/// Una parte de la copia en el modo sencillo.
#[derive(Debug, Clone)]
pub struct PlanItem {
    /// Lo que ve la persona: «Windows (NTFS) · 200 GiB».
    pub title: String,
    pub device: PathBuf,
    pub btrfs: bool,
    pub size: u64,
    pub fstype: Option<String>,
    /// Disco al que pertenece y posición en su tabla de particiones; los
    /// rellena la interfaz para escribir el conjunto de copia.
    pub disk: Option<PathBuf>,
    pub index: Option<usize>,
}

impl PlanItem {
    pub fn new(v: &Volume) -> Self {
        Self {
            title: format!("{} · {}", volume_title(v), devices::human_size(v.size)),
            device: v.path.clone(),
            btrfs: v.is_btrfs(),
            size: v.size,
            fstype: v.fstype.clone(),
            disk: None,
            index: None,
        }
    }

    /// La misma parte, situada en la tabla de particiones de `disk`.
    pub fn on_disk(mut self, disk: &Path) -> Self {
        self.index = crate::restore::table_index(disk, &self.device);
        self.disk = Some(disk.to_path_buf());
        self
    }

    /// Lo que puede ocupar la copia, antes de comprimir: lo usado en un
    /// Btrfs; la partición entera en el resto (imagen sector a sector).
    pub fn max_bytes(&self, size: u64) -> u64 {
        if self.btrfs {
            Superblock::read_from(&self.device)
                .map(|sb| sb.bytes_used)
                .unwrap_or(size)
        } else {
            size
        }
    }
}

/// Espacio que hace falta en el destino, en el peor caso (sin comprimir).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceNeed {
    /// Las copias, que se quedan.
    pub copies: u64,
    /// El mayor temporal de comprobación: cada parte se restaura en el
    /// propio destino para comprobarla y se borra al terminar.
    pub scratch: u64,
}

impl SpaceNeed {
    /// `parts`: lo que puede ocupar cada parte antes de comprimir.
    pub fn of(parts: &[u64]) -> Self {
        Self {
            copies: parts.iter().sum(),
            scratch: parts.iter().copied().max().unwrap_or(0),
        }
    }

    pub fn total(&self) -> u64 {
        self.copies.saturating_add(self.scratch)
    }

    pub fn fit(&self, free: Option<u64>) -> Fit {
        match free {
            None => Fit::Unknown,
            Some(f) if f >= self.total() => Fit::Fits,
            // Ni comprimiendo: el temporal de la mayor parte ya no entra.
            Some(f) if f < self.scratch => Fit::No,
            Some(_) => Fit::Tight,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fit {
    Fits,
    /// Depende de cuánto comprima.
    Tight,
    No,
    Unknown,
}

/// Bytes libres del destino. Si no está montado, se monta un momento en
/// solo lectura para medirlo y se desmonta.
pub fn measure_free(dest: &Destination) -> Option<u64> {
    if let Some(m) = &dest.mountpoint {
        return free_bytes(m);
    }
    let mut s = Session::new(false);
    let name = dest.device.file_name()?.to_string_lossy().into_owned();
    let mnt = PathBuf::from(RUN_DIR).join("medir").join(name);
    std::fs::create_dir_all(&mnt).ok()?;
    s.run(
        Cmd::new("mount")
            .arg("-o")
            .arg("ro")
            .arg(&dest.device)
            .arg(&mnt),
    )
    .ok()?;
    let free = free_bytes(&mnt);
    let _ = s.run_unchecked(&Cmd::new("umount").arg(&mnt));
    free
}

/// Aviso de una partición antes de copiarla: BitLocker, NTFS sin cerrar o
/// NTFS que no se pudo comprobar. El origen solo se lee.
pub fn volume_warning(v: &Volume) -> Option<String> {
    if v.is_bitlocker() {
        return Some(
            "Cifrada con BitLocker: se copia tal cual y no se puede comprobar por dentro. \
             Ten a mano la clave de recuperación"
                .into(),
        );
    }
    if v.fstype.as_deref() == Some("ntfs") {
        use partshield_core::raw::{ntfs_state, NtfsState};
        return match ntfs_state(&mut Session::new(false), &v.path) {
            NtfsState::Clean => None,
            NtfsState::NotClean(why) | NtfsState::Unknown(why) => Some(why),
        };
    }
    None
}

/// El resultado de una parte: su informe y dónde se guardó, o el error.
pub type PartResult = Result<(Report, PathBuf), String>;

/// Lo que va pasando mientras se copia, para la barra de progreso.
pub enum PlanEvent {
    /// Empieza la parte `index` (desde 0).
    Part(usize),
    /// Comando que se va a ejecutar.
    Command(String),
    /// Ha terminado la parte `index`.
    Done(usize, Box<PartResult>),
    /// Tabla de particiones guardada (o por qué no).
    Table(Result<PathBuf, String>),
    /// Conjunto de copia escrito (o por qué no).
    Set(Result<PathBuf, String>),
    /// No queda nada por hacer.
    Finished,
}

/// Un disco del que se copia algo, para su conjunto de copia.
#[derive(Debug, Clone)]
pub struct PlanDisk {
    pub path: PathBuf,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size: u64,
    /// Se copia entero: se guarda también su tabla de particiones.
    pub whole: bool,
}

impl PlanDisk {
    pub fn new(d: &Disk, whole: bool) -> Self {
        Self {
            path: d.path.clone(),
            model: d.model.clone(),
            serial: d.serial.clone(),
            size: d.size,
            whole,
        }
    }
}

/// Copia y verifica cada parte, una detrás de otra. Un fallo en una no
/// para las demás: cada una tiene su propio veredicto. De un disco entero
/// guarda antes su tabla de particiones (`sfdisk --dump`). Al final escribe
/// un conjunto de copia por disco: lo que lista «Restaurar».
pub fn run_plan(
    items: &[PlanItem],
    disks: &[PlanDisk],
    dest: &Destination,
    events: Sender<PlanEvent>,
) {
    let stamp = chrono::Local::now();
    let mut tables: Vec<Option<PathBuf>> = Vec::new();
    for disk in disks {
        if !disk.whole {
            tables.push(None);
            continue;
        }
        let r = save_partition_table(&disk.path, dest);
        tables.push(r.as_ref().ok().cloned());
        let _ = events.send(PlanEvent::Table(r.map_err(|e| format!("{e:#}"))));
    }
    let mut parts: Vec<Option<SetPart>> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let _ = events.send(PlanEvent::Part(i));
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let fwd = {
            let events = events.clone();
            std::thread::spawn(move || {
                for c in rx {
                    let _ = events.send(PlanEvent::Command(c));
                }
            })
        };
        let mut default_subvolume = None;
        let r = run_item(item, dest, tx, &mut default_subvolume).map_err(|e| format!("{e:#}"));
        let _ = fwd.join();
        parts.push(Some(set_part(item, &r, default_subvolume)));
        let _ = events.send(PlanEvent::Done(i, Box::new(r)));
    }
    for (disk, table) in disks.iter().zip(tables) {
        let mine: Vec<SetPart> = items
            .iter()
            .zip(parts.iter_mut())
            .filter(|(it, _)| it.disk.as_deref() == Some(disk.path.as_path()))
            .filter_map(|(_, p)| p.take())
            .collect();
        if mine.is_empty() {
            continue;
        }
        let set = BackupSet {
            version: 1,
            created: stamp.to_rfc3339(),
            disk: SetDisk {
                path: disk.path.display().to_string(),
                model: disk.model.clone(),
                serial: disk.serial.clone(),
                size: disk.size,
            },
            table: table.and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned())),
            parts: mine,
        };
        let r = save_set(&set, &disk.path, dest, &stamp).map_err(|e| format!("{e:#}"));
        let _ = events.send(PlanEvent::Set(r));
    }
    let _ = events.send(PlanEvent::Finished);
}

fn set_part(item: &PlanItem, r: &PartResult, default_subvolume: Option<String>) -> SetPart {
    let (report, verdict, error) = match r {
        Ok((rep, path)) => (
            path.file_name().map(|n| n.to_string_lossy().into_owned()),
            match (&rep.error, rep.verdict()) {
                (Some(_), _) | (_, Verdict::Invalid) => SetVerdict::Invalid,
                (_, Verdict::Reduced) => SetVerdict::Reduced,
                (_, Verdict::Valid) => SetVerdict::Valid,
            },
            rep.error.clone(),
        ),
        Err(e) => (None, SetVerdict::Invalid, Some(e.clone())),
    };
    SetPart {
        index: item.index,
        device: item.device.display().to_string(),
        title: item.title.clone(),
        size: item.size,
        fstype: item.fstype.clone(),
        report,
        verdict,
        error,
        default_subvolume,
    }
}

fn save_set(
    set: &BackupSet,
    disk: &Path,
    dest: &Destination,
    stamp: &chrono::DateTime<chrono::Local>,
) -> Result<PathBuf> {
    let mut s = Session::new(false);
    let dir = prepare_destination(dest, &mut s)?;
    let name = disk
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "disco".into());
    set.save(&dir, &format!("{name}-{}", stamp.format("%Y%m%d-%H%M%S")))
}

/// Btrfs por `btrfs send` (solo datos, todos los subvolúmenes que no son
/// snapshots) si es seguro; si no, y para todo lo demás, imagen sector a
/// sector, que vale para cualquier contenido y se compara bit a bit.
fn run_item(
    item: &PlanItem,
    dest: &Destination,
    tx: Sender<String>,
    default_subvolume: &mut Option<String>,
) -> Result<(Report, PathBuf)> {
    let opts = JobOptions {
        dest: dest.clone(),
        zstd_level: 3,
        hash: HashAlgo::Blake3,
    };
    let raw = Job::Raw {
        device: item.device.clone(),
    };
    if !item.btrfs {
        return run_job(&raw, &opts, tx);
    }
    let info = detect(&item.device)?;
    let allowed = |m: Mode| info.modes.iter().any(|a| a.mode == m && a.allowed);
    if !allowed(Mode::Send) {
        if allowed(Mode::Raw) {
            return run_job(&raw, &opts, tx);
        }
        bail!("ningún modo de copia es seguro para este Btrfs; míralo en el modo avanzado");
    }
    let m = mount_source(&item.device, &info.superblock.fsid)?;
    *default_subvolume = default_subvolume_of(&m.mountpoint);
    let mut subvolumes: Vec<String> = m
        .subvolumes
        .iter()
        .filter(|s| s.selected_by_default())
        .map(|s| s.path.clone())
        .collect();
    if m.subvolumes.is_empty() {
        subvolumes.push(String::new());
    }
    let job = Job::Send {
        mount: m.mountpoint.clone(),
        subvolumes,
        layout: true,
        keep_snapshot: false,
    };
    let r = run_job(&job, &opts, tx);
    let _ = Session::new(false).run_unchecked(&Cmd::new("umount").arg(&m.mountpoint));
    r
}

/// Ruta del subvolumen por defecto de un Btrfs montado, o `None` si es el
/// nivel superior: `ID 256 gen 7 top level 5 path @` → `@`.
fn default_subvolume_of(mount: &Path) -> Option<String> {
    let out = Session::new(false)
        .run(
            Cmd::new("btrfs")
                .arg("subvolume")
                .arg("get-default")
                .arg(mount),
        )
        .ok()?;
    parse_default_subvolume(&out)
}

fn parse_default_subvolume(out: &str) -> Option<String> {
    let (_, path) = out.trim().split_once(" path ")?;
    Some(path.trim().to_string())
}

fn save_partition_table(disk: &Path, dest: &Destination) -> Result<PathBuf> {
    let mut s = Session::new(false);
    let dir = prepare_destination(dest, &mut s)?;
    std::fs::create_dir_all(&dir)?;
    let dump = s.run(Cmd::new("sfdisk").arg("--dump").arg(disk))?;
    let name = disk
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "disco".into());
    let path = dir.join(format!(
        "{name}-{}.sfdisk",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&path, dump)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_mode_hides_live_usb_and_picks_internal_disk() {
        let disks = devices::parse(include_str!("../tests/lsblk-zenbook.json")).unwrap();
        let shown = basic_disks(&disks, false);
        let paths: Vec<_> = shown.iter().map(|d| d.path.display().to_string()).collect();
        assert_eq!(paths, ["/dev/nvme0n1", "/dev/nvme1n1"]);
        assert_eq!(default_disk(&shown), Some(PathBuf::from("/dev/nvme0n1")));
        assert_eq!(
            disk_title(shown[0]),
            "Disco interno NVMe · Samsung SSD 980 PRO 1TB · 954 GiB"
        );
        let titles: Vec<_> = shown[0]
            .volumes
            .iter()
            .filter(|v| v.depth == 0)
            .map(volume_title)
            .collect();
        assert_eq!(titles, ["Arranque (EFI)", "Cifrada (LUKS)"]);
    }

    #[test]
    fn space_counts_copies_plus_the_largest_scratch() {
        let gib = 1u64 << 30;
        // Windows 295 + Btrfs con 420 usados + EFI.
        let need = SpaceNeed::of(&[295 * gib, 420 * gib, gib / 2]);
        assert_eq!(need.copies, 715 * gib + gib / 2);
        assert_eq!(need.scratch, 420 * gib);
        assert_eq!(need.fit(Some(2000 * gib)), Fit::Fits);
        assert_eq!(need.fit(Some(932 * gib)), Fit::Tight);
        assert_eq!(need.fit(Some(400 * gib)), Fit::No);
        assert_eq!(need.fit(None), Fit::Unknown);
        assert_eq!(SpaceNeed::of(&[]).total(), 0);
    }

    #[test]
    fn friendly_names_for_common_partitions() {
        let v = |fstype: Option<&str>, size: u64, label: Option<&str>| Volume {
            path: PathBuf::from("/dev/x"),
            kind: "part".into(),
            depth: 0,
            size,
            fstype: fstype.map(String::from),
            label: label.map(String::from),
            uuid: None,
            parttype: None,
            mountpoints: vec![],
        };
        let gib = 1u64 << 30;
        assert_eq!(
            volume_title(&v(Some("ntfs"), 200 * gib, None)),
            "Windows (NTFS)"
        );
        assert_eq!(
            volume_title(&v(Some("ntfs"), gib / 2, None)),
            "Recuperación de Windows"
        );
        assert_eq!(
            volume_title(&v(Some("btrfs"), 600 * gib, Some("arch"))),
            "Linux (Btrfs) «arch»"
        );
        assert_eq!(
            volume_title(&v(Some("ext4"), 9 * gib, None)),
            "Linux (ext4)"
        );
        assert_eq!(volume_title(&v(None, gib, None)), "Sin formato");
        let mut msr = v(None, gib / 64, None);
        msr.parttype = Some("e3c9e316-0b5c-4db8-817d-f92df00215ae".into());
        assert_eq!(volume_title(&msr), "Reservada de Microsoft (MSR)");
        assert!(!checked_with_disk(&msr));
        assert_eq!(
            volume_title(&v(Some("BitLocker"), 200 * gib, None)),
            "Windows cifrado (BitLocker)"
        );
        assert!(!checked_with_disk(&v(Some("swap"), gib, None)));
        assert!(checked_with_disk(&v(Some("ntfs"), gib, None)));
    }

    #[test]
    fn parses_subvolume_list() {
        let out = "ID 256 gen 1520 top level 5 path @\n\
                   ID 257 gen 1519 top level 5 path @home\n\
                   ID 258 gen 1400 top level 256 path @/var/lib/machines\n\
                   ID 259 gen 1500 top level 5 path @snapshots\n\
                   ID 260 gen 1500 top level 259 path @snapshots/.snapshots/1/snapshot\n\
                   ID 261 gen 1510 top level 5 path my dir with spaces\n";
        let v = parse_subvolume_list(out);
        assert_eq!(v.len(), 6);
        assert_eq!(v[1].path, "@home");
        assert_eq!(v[2].top_level, 256);
        assert_eq!(v[5].path, "my dir with spaces");
        let chosen: Vec<_> = v
            .iter()
            .filter(|s| s.selected_by_default())
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(chosen, ["@", "@home", "my dir with spaces"]);
    }

    #[test]
    fn labels_are_stable_names() {
        assert_eq!(subvol_label("@home"), "@home");
        assert_eq!(subvol_label("@/var/lib"), "@_var_lib");
        assert_eq!(subvol_label(""), "nivel-superior");
    }

    /// Copia real con loops: Btrfs con @ y @home → ext4. Necesita root;
    /// sin root (o sin loops) no hace nada.
    #[test]
    fn copy_job_end_to_end_on_loops() {
        if !nix_root() || std::env::var_os("PARTSHIELD_GUI_E2E").is_none() {
            eprintln!("omitida: PARTSHIELD_GUI_E2E=1 y root");
            return;
        }
        std::env::set_var("PARTSHIELD_SANDBOX", "1");
        let dir = tempfile::tempdir().unwrap();
        let sh = |c: &str| {
            let o = std::process::Command::new("sh")
                .arg("-c")
                .arg(c)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{c}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        let d = dir.path().display();
        let src = sh(&format!(
            "truncate -s 1G {d}/src.img && losetup -f --show {d}/src.img"
        ));
        let dst = sh(&format!(
            "truncate -s 2G {d}/dst.img && losetup -f --show {d}/dst.img"
        ));
        let cleanup = |m: &str| {
            let _ = std::process::Command::new("umount").arg(m).status();
        };
        sh(&format!(
            "mkfs.btrfs -q -L demo {src} && mkfs.ext4 -q -L copias {dst}"
        ));
        sh(&format!(
            "m=$(mktemp -d) && mount {src} $m && btrfs -q subvolume create $m/@ $m/@home && \
             mkdir -p $m/@home/ana && echo hola > $m/@home/ana/nota.txt && \
             head -c 3M /dev/urandom > $m/@/datos.bin && umount $m"
        ));
        let fsid = sh(&format!("blkid -s UUID -o value {src}"));
        let m = mount_source(Path::new(&src), &fsid).unwrap();
        let paths: Vec<_> = m.subvolumes.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["@", "@home"]);
        let dest = Destination {
            device: PathBuf::from(&dst),
            disk: "loop".into(),
            fstype: "ext4".into(),
            label: None,
            size: 2 << 30,
            mountpoint: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let job = Job::Send {
            mount: m.mountpoint.clone(),
            subvolumes: vec!["@".into(), "@home".into()],
            layout: false,
            keep_snapshot: false,
        };
        let res = run_job(
            &job,
            &JobOptions {
                dest,
                zstd_level: 3,
                hash: HashAlgo::Blake3,
            },
            tx,
        );
        let seen: Vec<String> = rx.try_iter().collect();
        let dmnt = format!(
            "/run/partshield/destino/{}",
            Path::new(&dst).file_name().unwrap().to_string_lossy()
        );
        let (report, path) = res.unwrap();
        // El informe guardado también lleva los montajes de preparación
        // (se lee antes de desmontar el destino).
        let saved = std::fs::read_to_string(&path).unwrap_or_default();
        cleanup(&m.mountpoint.display().to_string());
        assert!(path.starts_with(&dmnt), "{}", path.display());
        cleanup(&dmnt);
        let _ = sh(&format!("losetup -d {src} {dst}"));
        assert_eq!(report.error, None);
        assert_eq!(report.verdict(), partshield_core::types::Verdict::Valid);
        assert_eq!(report.subvolumes.len(), 2);
        assert!(seen.iter().any(|c| c.starts_with("btrfs send")), "{seen:?}");
        assert!(report.commands[0].command.starts_with("mount"));
        assert!(
            saved.contains("remount,rw"),
            "falta el montaje en el informe guardado"
        );
    }

    /// Modo sencillo con loops, de punta a punta: copiar un disco con tabla
    /// GPT, una ext4 (sector a sector) y un Btrfs con @ y @home (send), y
    /// restaurar el conjunto en otro disco. Necesita root y
    /// PARTSHIELD_GUI_E2E=1.
    #[test]
    fn whole_disk_backup_and_restore_on_loops() {
        use crate::restore::{run_restore, BackupSet, RestoreEvent};
        if !nix_root() || std::env::var_os("PARTSHIELD_GUI_E2E").is_none() {
            eprintln!("omitida: PARTSHIELD_GUI_E2E=1 y root");
            return;
        }
        std::env::set_var("PARTSHIELD_SANDBOX", "1");
        let dir = tempfile::tempdir().unwrap();
        let try_sh = |c: &str| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(c)
                .output()
                .unwrap()
        };
        let sh = |c: &str| {
            let o = try_sh(c);
            assert!(
                o.status.success(),
                "{c}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8(o.stdout).unwrap().trim().to_string()
        };
        let d = dir.path().display();
        sh(&format!(
            "truncate -s 600M {d}/src.img {d}/new.img && truncate -s 2G {d}/dst.img && \
             printf 'label: gpt\\n,64M\\n,\\n' | sfdisk -q {d}/src.img"
        ));
        // Sin udev ni partscan (contenedores), cada partición va a su propio
        // loop con --offset; vale igual para el disco nuevo, que aún no
        // tiene tabla.
        let table: serde_json::Value =
            serde_json::from_str(&sh(&format!("sfdisk -J {d}/src.img"))).unwrap();
        let part = |img: &str, n: usize| -> String {
            let p = &table["partitiontable"]["partitions"][n];
            sh(&format!(
                "losetup -f --show --offset {} --sizelimit {} {d}/{img}",
                p["start"].as_u64().unwrap() * 512,
                p["size"].as_u64().unwrap() * 512
            ))
        };
        let src = sh(&format!("losetup -f --show {d}/src.img"));
        let dst = sh(&format!("losetup -f --show {d}/dst.img"));
        let (p1, p2) = (part("src.img", 0), part("src.img", 1));
        sh(&format!(
            "mkfs.ext4 -q -L arranque {p1} && mkfs.btrfs -q -L linux {p2} && \
             mkfs.ext4 -q -L copias {dst} && \
             m=$(mktemp -d) && mount {p2} $m && btrfs -q subvolume create $m/@ $m/@home && \
             mkdir -p $m/@home/ana && echo hola > $m/@home/ana/nota.txt && \
             head -c 2M /dev/urandom > $m/@/datos.bin && cp $m/@/datos.bin {d}/datos.bin && \
             umount $m"
        ));
        let uuid = sh(&format!("blkid -s UUID -o value {p2}"));
        let p1_hash = sh(&format!("sha256sum < {p1}"));

        // ---- copia
        let vol = |path: &str, fstype: &str, size: u64| Volume {
            path: PathBuf::from(path),
            kind: "part".into(),
            depth: 0,
            size,
            fstype: Some(fstype.into()),
            label: None,
            uuid: None,
            parttype: None,
            mountpoints: vec![],
        };
        let located = |v: Volume, index: usize| {
            let mut it = PlanItem::new(&v);
            it.disk = Some(PathBuf::from(&src));
            it.index = Some(index);
            it
        };
        let items = [
            located(vol(&p1, "ext4", 64 << 20), 0),
            located(vol(&p2, "btrfs", 535 << 20), 1),
        ];
        let disks = [PlanDisk {
            path: PathBuf::from(&src),
            model: Some("Disco de pruebas".into()),
            serial: Some("SERIE1234".into()),
            size: 600 << 20,
            whole: true,
        }];
        let dest = Destination {
            device: PathBuf::from(&dst),
            disk: "loop".into(),
            fstype: "ext4".into(),
            label: None,
            size: 2 << 30,
            mountpoint: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        run_plan(&items, &disks, &dest, tx);
        let mut done = Vec::new();
        let mut set_path = None;
        let mut finished = false;
        for ev in rx.try_iter() {
            match ev {
                PlanEvent::Done(i, r) => done.push((i, *r)),
                PlanEvent::Set(r) => set_path = Some(r.unwrap()),
                PlanEvent::Finished => finished = true,
                _ => {}
            }
        }
        let dmnt = format!(
            "/run/partshield/destino/{}",
            Path::new(&dst).file_name().unwrap().to_string_lossy()
        );
        let cleanup = |extra: &[&str]| {
            let _ = try_sh(&format!("umount {dmnt}"));
            let _ = try_sh(&format!(
                "losetup -d {src} {dst} {p1} {p2} {}",
                extra.join(" ")
            ));
        };
        if done.len() != 2 || !finished || set_path.is_none() {
            cleanup(&[]);
            panic!("la copia no terminó: {} partes", done.len());
        }
        for (i, r) in &done {
            let ok = r
                .as_ref()
                .is_ok_and(|(rep, _)| rep.error.is_none() && rep.verdict() == Verdict::Valid);
            if !ok {
                cleanup(&[]);
                panic!(
                    "parte {i}: {:?}",
                    r.as_ref().map(|(rep, _)| rep.verification.clone())
                );
            }
        }
        let set_path = set_path.unwrap();
        let set = BackupSet::load(&set_path).unwrap();
        assert_eq!(set.verdict(), SetVerdict::Valid);
        assert_eq!(set.not_restorable(), None);
        assert_eq!(set.disk.serial.as_deref(), Some("SERIE1234"));
        assert_eq!(set.parts.len(), 2);
        assert_eq!(set.parts[1].default_subvolume, None);

        // ---- restauración en un disco nuevo. El original se desconecta:
        // dos Btrfs con el mismo UUID a la vez no pueden convivir.
        let _ = try_sh(&format!("losetup -d {src} {p1} {p2}"));
        let new = sh(&format!("losetup -f --show {d}/new.img"));
        let (n1, n2) = (part("new.img", 0), part("new.img", 1));
        let restore = |set_path: &Path| {
            let (tx, rx) = std::sync::mpsc::channel();
            run_restore(
                set_path,
                Path::new(&new),
                Some(vec![PathBuf::from(&n1), PathBuf::from(&n2)]),
                tx,
            );
            rx.try_iter()
                .find_map(|e| match e {
                    RestoreEvent::Finished(r) => Some(r),
                    _ => None,
                })
                .expect("sin evento final")
        };

        // Un archivo de la copia dañado: no se escribe nada en el disco.
        let stream = sh(&format!("ls {dmnt}/partshield-copias/*.raw.zst"));
        sh(&format!(
            "cp {stream} {d}/bueno.zst && printf 'X' | dd of={stream} bs=1 seek=100 conv=notrunc status=none"
        ));
        let bad = restore(&set_path);
        let untouched = try_sh(&format!("sfdisk -J {new}"));
        sh(&format!("cp {d}/bueno.zst {stream}"));
        let good = restore(&set_path);

        let checks = (|| -> Result<(), String> {
            let e = bad.as_ref().err().ok_or("la copia dañada se restauró")?;
            if !e.contains("no se ha escrito nada") {
                return Err(format!("mensaje inesperado: {e}"));
            }
            if untouched.status.success() {
                return Err("el disco tenía tabla tras la restauración fallida".into());
            }
            good.as_ref().map_err(|e| format!("restaurar falló: {e}"))?;
            let parts = |img: &str| -> Vec<(u64, u64, String, String)> {
                let v: serde_json::Value =
                    serde_json::from_str(&sh(&format!("sfdisk -J {d}/{img}"))).unwrap();
                v["partitiontable"]["partitions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        (
                            p["start"].as_u64().unwrap(),
                            p["size"].as_u64().unwrap(),
                            p["type"].as_str().unwrap().to_string(),
                            p["uuid"].as_str().unwrap().to_string(),
                        )
                    })
                    .collect()
            };
            if parts("src.img") != parts("new.img") {
                return Err("la tabla restaurada no es igual".into());
            }
            if sh(&format!("sha256sum < {n1}")) != p1_hash {
                return Err("la ext4 restaurada no es idéntica".into());
            }
            if sh(&format!("blkid -s UUID -o value {n2}")) != uuid {
                return Err("el Btrfs restaurado no tiene el UUID original".into());
            }
            let out = sh(&format!(
                "m=$(mktemp -d) && mount -o ro {n2} $m && \
                 cmp $m/@/datos.bin {d}/datos.bin && cat $m/@home/ana/nota.txt && \
                 btrfs subvolume list $m | awk '{{print $NF}}' | sort | tr '\\n' ' '; umount $m"
            ));
            if out != "hola\n@ @home" {
                return Err(format!("contenido del Btrfs restaurado: {out:?}"));
            }
            Ok(())
        })();
        cleanup(&[&new, &n1, &n2]);
        checks.unwrap();
    }

    #[test]
    fn default_subvolume_is_none_for_the_top_level() {
        assert_eq!(parse_default_subvolume("ID 5 (FS_TREE)\n"), None);
        assert_eq!(
            parse_default_subvolume("ID 256 gen 7 top level 5 path @\n").as_deref(),
            Some("@")
        );
    }

    fn nix_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .map(|o| o.stdout == b"0\n")
            .unwrap_or(false)
    }

    #[test]
    fn destinations_skip_source_disk_and_live_usb() {
        let disks = devices::parse(include_str!("../tests/lsblk-zenbook.json")).unwrap();
        let src = PathBuf::from("/dev/mapper/luks-c0ffee");
        let d = destinations(&disks, &[src]);
        let devs: Vec<_> = d.iter().map(|x| x.device.display().to_string()).collect();
        assert_eq!(devs, ["/dev/nvme1n1p1"]);
        assert_eq!(d[0].fstype, "ext4");
        assert!(d[0].mountpoint.is_none());
    }
}
