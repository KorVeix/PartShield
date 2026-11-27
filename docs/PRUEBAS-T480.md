# Pruebas en hardware real — ThinkPad T480

Batería manual para dar PartShield por listo para pruebas intensivas. Se hace
desde el USB de PartShield sobre un equipo de pruebas, no con loop devices:
las reglas de `CONTRIBUTING.md` (sandbox, solo `/dev/loop*`) siguen valiendo
para el código y los tests automáticos.

## Equipo

- Lenovo ThinkPad T480, SSD de 256 GB, arranque dual.
- Particiones: ESP (FAT32), Windows 11 (NTFS) y CachyOS (Btrfs con
  subvolúmenes).
- Destino: disco externo con espacio de sobra.
- Firmware de fábrica. Libreboot queda fuera hasta terminar esta batería;
  después se repite C11 y C12.

## Lo que ya se sabe antes de empezar

- NTFS sin cerrar (Windows hibernado o con Fast Startup) y BitLocker se
  detectan: la interfaz avisa antes de copiar y la copia sale 🟡 con el
  motivo, nunca 🟢. Probado con loops (un `hiberfil.sys` con la marca
  «hibr»); C3 y C4 lo confirman con un Windows de verdad.
- Un disco entero en raw es **un solo archivo** (`.zst`). La verificación
  entra en cada partición de la imagen (fsck y montaje): puede salir 🟢. Si
  hay una partición cifrada o un NTFS sin cerrar, sale 🟡. C11 lo comprueba.
- Restaurar un disco entero copiado por particiones no es una sola acción:
  tabla y ESP con `restore-layout`, NTFS en raw y Btrfs con `mkfs.btrfs`,
  `restore` y UUID. La restauración desde la GUI está pendiente.

## Preparación

- **P1 · Windows.** `manage-bde -status C:` como administrador. Si BitLocker
  está activo, guardar la clave de recuperación antes de seguir.
- **P2 · CachyOS.** Guardar la salida de `lsblk -f`,
  `sudo btrfs subvolume list /` y el gestor de arranque en uso. Es la
  referencia para comparar tras restaurar.
- **P3 · Testigos.** Crear un archivo con la fecha y la hora en Windows, en
  la ESP, en `/` y en `/home`. Es una comprobación independiente de
  PartShield.

## Casos

| # | Caso | Se espera | Superado si |
| --- | --- | --- | --- |
| C1 | ESP FAT32 en raw | 🟢 | fsck, montaje e idéntica bit a bit |
| C2 | NTFS con Windows apagado del todo (Fast Startup desactivado) | 🟢 | Ídem |
| C3 | NTFS con Fast Startup activado | 🟡 | Aviso antes de copiar y «estructura» omitida con el motivo. Si da 🟢, fallo |
| C4 | NTFS con BitLocker (si está activo) | 🟡 | Sale como «Windows cifrado (BitLocker)», con aviso; idéntica bit a bit |
| C5 | Btrfs de CachyOS, todos los subvolúmenes | 🟢 | Pasan los cinco pasos |
| C6 | Disco entero desde la GUI (tabla + particiones) | 🟢 | Informe con el `.sfdisk` y cada partición |
| C7 | Btrfs incremental (`--parent`) tras cambiar archivos | 🟢 | La cadena se verifica |
| C8 | Corromper 1 byte de un stream y ejecutar `verify` | 🔴 | Lo detecta |
| C9 | Desenchufar el destino a mitad de copia | 🔴 | No queda un informe «válido» a medias |
| C10 | Restaurar por particiones (C6) sobre el SSD borrado | Arrancan los dos | Windows y CachyOS arrancan, están los testigos y no se pide la clave de BitLocker |
| C11 | Disco entero en un solo archivo (raw) | 🟢 (🟡 con BitLocker) | Idéntica bit a bit, y fsck y montaje de cada partición |
| C12 | Restaurar C11 sobre el SSD borrado | Arrancan los dos | Igual que C10 |

Orden: P1–P3, C1, C2, C5, C11, C6, C7, C8, C9 y al final C12 y C10, que
borran el SSD.

## Comandos de referencia

Ajustar los dispositivos con `lsblk` antes de cada caso. Desde el USB de
PartShield nada del SSD está montado.

```sh
# C1, C2: partición en raw
sudo partshield backup --mode raw /dev/nvme0n1p1 --dest /mnt/usb/t480
sudo partshield backup --mode raw /dev/nvme0n1p3 --dest /mnt/usb/t480

# C5: send necesita crear snapshots, así que se monta la raíz del Btrfs
# (subvolid=5) en lectura-escritura y se copian los subvolúmenes con la
# tabla y la ESP. Añadir un --subvol por cada uno (@root, @log, @cache…).
sudo mount -o subvolid=5 /dev/nvme0n1pX /mnt/sis
sudo partshield backup --subvol @=/mnt/sis/@ --subvol @home=/mnt/sis/@home \
    --layout --dest /mnt/usb/t480 --keep-snapshot

# C8: volver a verificar tras dañar un byte
sudo partshield verify --report /mnt/usb/t480/XXXX.report.json

# C11: disco entero en un solo archivo
sudo partshield backup --mode raw /dev/nvme0n1 --dest /mnt/usb/t480

# C12: devolverlo al SSD
sudo partshield restore --report /mnt/usb/t480/XXXX.report.json --to /dev/nvme0n1
```

## Resultados

| # | Fecha | ISO | Resultado | Notas |
| --- | --- | --- | --- | --- |
| | | | | |
