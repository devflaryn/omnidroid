#!/usr/bin/env python3
"""Turn the pinned AOSP API 35 arm64 system image into omnidroid's sysroot.

Runs on Linux (needs `debugfs`; `fsck.erofs` only if an APEX payload is erofs). Output:
  <out>/objects/<sha256[:2]>/<sha256>   every regular file, stored by content: guest paths can differ
                                        only by case (seven ringtones do) or be names Windows reserves,
                                        and symlinks are NOT created (Windows cannot hold them)
  <out>/sysroot.manifest    every directory, file (mode, size, sha256) and symlink (target)

    python3 tools/make_sysroot.py --zip arm64-v8a-35_r02.zip --out sysroot/aosp-35
    python3 tools/make_sysroot.py --verify sysroot/aosp-35
"""
import argparse, hashlib, io, os, shutil, stat, struct, subprocess, sys, tempfile, zipfile

IMAGE_NAME = "arm64-v8a-35_r02.zip"
IMAGE_SHA1 = "2026a06409db630b56711afdbffb457c1dbaed49"
PARTITIONS = {"system": None, "system_ext": "/system_ext", "product": "/product", "vendor": "/vendor"}
EROFS_MAGIC = bytes.fromhex("e2e1f5e0")


def sha1_of(path):
    h = hashlib.sha1()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def gpt_partition(img, want="super"):
    """(byte offset, byte length) of the GPT partition named `want`, else the largest."""
    img.seek(512)
    hdr = img.read(92)
    if hdr[:8] != b"EFI PART":
        sys.exit("system.img: no GPT header at LBA 1")
    entries_lba, count, size = struct.unpack_from("<QII", hdr, 72)
    best = None
    for i in range(count):
        img.seek(entries_lba * 512 + i * size)
        e = img.read(size)
        first, last = struct.unpack_from("<QQ", e, 32)
        if first == 0:
            continue
        name = e[56:128].decode("utf-16-le").rstrip("\0")
        span = (first * 512, (last - first + 1) * 512)
        if name == want:
            return span
        if best is None or span[1] > best[1]:
            best = span
    return best


def lp_partitions(img, base):
    """{name: [(byte offset, byte length), ...]} from the LP metadata of a `super` at `base`."""
    img.seek(base + 4096)
    if img.read(4) != bytes.fromhex("67446c61"):
        sys.exit("super: no LP geometry magic")
    header_at = base + 4096 + 4096 * 2
    img.seek(header_at)
    h = img.read(128)
    if h[:4] != bytes.fromhex("30504c41"):
        sys.exit("super: no LP header magic")
    header_size, = struct.unpack_from("<I", h, 8)
    tables_size, = struct.unpack_from("<I", h, 44)
    (p_off, p_num, p_sz), (e_off, e_num, e_sz) = (struct.unpack_from("<III", h, 80),
                                                    struct.unpack_from("<III", h, 92))
    img.seek(header_at + header_size)
    t = img.read(tables_size)
    extents = [struct.unpack_from("<QIQI", t, e_off + i * e_sz) for i in range(e_num)]
    out = {}
    for i in range(p_num):
        e = t[p_off + i * p_sz: p_off + (i + 1) * p_sz]
        name = e[:36].split(b"\0")[0].decode()
        first, num = struct.unpack_from("<II", e, 40)
        out[name] = [(base + data * 512, sectors * 512)
                     for sectors, _kind, data, _dev in extents[first:first + num]]
    return out


def copy_extents(img, extents, dest):
    with open(dest, "wb") as out:
        for off, length in extents:
            img.seek(off)
            left = length
            while left:
                chunk = img.read(min(left, 1 << 20))
                out.write(chunk)
                left -= len(chunk)


def fs_kind(path):
    with open(path, "rb") as f:
        f.seek(1024)
        if f.read(4) == EROFS_MAGIC:
            return "erofs"
        f.seek(1024 + 0x38)
        if f.read(2) == b"\x53\xef":
            return "ext4"
    return "unknown"


def extract_fs(image, dest):
    os.makedirs(dest, exist_ok=True)
    kind = fs_kind(image)
    if kind == "ext4":
        subprocess.run(["debugfs", "-R", f"rdump / {dest}", image], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    elif kind == "erofs":
        tool = shutil.which("fsck.erofs") or os.path.expanduser("~/erofs/usr/bin/fsck.erofs")
        subprocess.run([tool, f"--extract={dest}", "--overwrite", "--preserve", image], check=True)
    else:
        sys.exit(f"{image}: neither ext4 nor erofs")


def apex_name(manifest_pb):
    """Field 1 (`name`) of apex_manifest.pb: tag 0x0a, varint length, bytes."""
    if manifest_pb[:1] != b"\x0a":
        return None
    n, shift, i = 0, 0, 1
    while True:
        b = manifest_pb[i]; i += 1
        n |= (b & 0x7F) << shift; shift += 7
        if not b & 0x80:
            break
    return manifest_pb[i:i + n].decode()


def extract_apexes(system_root, staging, mounts):
    apex_dir = os.path.join(system_root, "apex")
    for entry in sorted(os.listdir(apex_dir)):
        path = os.path.join(apex_dir, entry)
        if entry.endswith(".capex"):
            with zipfile.ZipFile(path) as z:
                data = z.read("original_apex")
        elif entry.endswith(".apex"):
            data = open(path, "rb").read()
        else:
            continue
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            name = apex_name(z.read("apex_manifest.pb")) or entry.rsplit(".", 1)[0]
            payload = os.path.join(staging, f"apex-{name}.img")
            with open(payload, "wb") as out:
                out.write(z.read("apex_payload.img"))
        dest = os.path.join(staging, "apex", name)
        extract_fs(payload, dest)
        mounts.append((dest, f"/apex/{name}"))


def walk(host_root, guest_root, lines, objects):
    for dirpath, dirnames, filenames in os.walk(host_root, followlinks=False):
        dirnames.sort()
        rel = os.path.relpath(dirpath, host_root)
        gdir = guest_root if rel == "." else f"{guest_root.rstrip('/')}/{rel.replace(os.sep, '/')}"
        lines.append(f"d\t{stat.S_IMODE(os.lstat(dirpath).st_mode):o}\t{gdir}")
        for name in sorted(dirnames + filenames):
            host = os.path.join(dirpath, name)
            guest = f"{gdir.rstrip('/')}/{name}"
            st = os.lstat(host)
            if stat.S_ISLNK(st.st_mode):
                lines.append(f"l\t{os.readlink(host)}\t{guest}")
                if name in dirnames:
                    dirnames.remove(name)  # never descend through a link
            elif stat.S_ISREG(st.st_mode):
                digest = hashlib.sha256(open(host, "rb").read()).hexdigest()
                dest = os.path.join(objects, digest[:2], digest)
                if not os.path.exists(dest):
                    os.makedirs(os.path.dirname(dest), exist_ok=True)
                    shutil.copyfile(host, dest)
                lines.append(f"f\t{stat.S_IMODE(st.st_mode):o}\t{st.st_size}\t{digest}\t{guest}")


def build(zip_path, out):
    if os.path.basename(zip_path) != IMAGE_NAME or sha1_of(zip_path) != IMAGE_SHA1:
        sys.exit(f"{zip_path}: not the pinned {IMAGE_NAME} (sha1 {IMAGE_SHA1})")
    staging = tempfile.mkdtemp(prefix="sysroot-", dir=os.path.dirname(os.path.abspath(out)) or ".")
    with zipfile.ZipFile(zip_path) as z:
        z.extract("arm64-v8a/system.img", staging)
    img_path = os.path.join(staging, "arm64-v8a", "system.img")
    mounts = []
    with open(img_path, "rb") as img:
        super_off, _ = gpt_partition(img)
        parts = lp_partitions(img, super_off)
        for name, guest in PARTITIONS.items():
            part_img = os.path.join(staging, f"{name}.img")
            copy_extents(img, parts[name], part_img)
            dest = os.path.join(staging, name)
            extract_fs(part_img, dest)
            if name == "system":
                # System-as-root: the partition's root is `/`, and `/system` is a directory in it.
                root = os.path.join(dest, "system") if os.path.isdir(os.path.join(dest, "system")) else dest
                mounts.append((root, "/system"))
                system_root = root
            else:
                mounts.append((dest, guest))
    extract_apexes(system_root, staging, mounts)
    objects = os.path.join(out, "objects")
    os.makedirs(objects, exist_ok=True)
    lines = [f"# omnidroid sysroot v1 image={IMAGE_NAME} sha1={IMAGE_SHA1}", "d\t755\t/", "d\t755\t/apex"]
    for host_root, guest_root in mounts:
        walk(host_root, guest_root, lines, objects)
    # Sorted by guest path, so the manifest (and its pinned sha256) is the same whatever order the
    # extraction filesystem lists directories in.
    lines = lines[:1] + sorted(set(lines[1:]), key=lambda line: line.rsplit("	", 1)[1])
    manifest = os.path.join(out, "sysroot.manifest")
    with open(manifest, "w", newline="\n") as f:
        f.write("\n".join(lines) + "\n")
    shutil.rmtree(staging)
    print("sysroot.manifest sha256", hashlib.sha256(open(manifest, "rb").read()).hexdigest())
    print("entries", len(lines) - 1)


def verify(out):
    bad = 0
    for line in open(os.path.join(out, "sysroot.manifest"), encoding="utf-8"):
        parts = line.rstrip("\n").split("\t")
        if parts[0] != "f":
            continue
        _, _mode, size, digest, guest = parts
        path = os.path.join(out, "objects", digest[:2], digest)
        if not os.path.isfile(path) or os.path.getsize(path) != int(size) \
                or hashlib.sha256(open(path, "rb").read()).hexdigest() != digest:
            print("MISMATCH", guest); bad += 1
    print("verified" if not bad else f"{bad} mismatches")
    return 1 if bad else 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--zip"); ap.add_argument("--out"); ap.add_argument("--verify")
    a = ap.parse_args()
    sys.exit(verify(a.verify) if a.verify else build(a.zip, a.out))
