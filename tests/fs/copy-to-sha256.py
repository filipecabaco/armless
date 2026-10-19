#!/usr/bin/env python3

import os
import logging
import stat
import argparse
import hashlib
import shutil
import tarfile
import sys
import io
import importlib

import cdc

# keep in sync with HASH_LENGTH in fs2json.py
HASH_LENGTH = 10

def hash_file(filename) -> str:
    with open(filename, "rb", buffering=0) as f:
        return hash_fileobj(f)

def hash_fileobj(f) -> str:
    h = hashlib.sha256()
    for b in iter(lambda: f.read(128*1024), b""):
        h.update(b)
    return h.hexdigest()

class ZstdCompress():
    def __init__(self):
        self.zstd_lib = None
        self.zstd_builtin = False

        if sys.version_info >= (3, 14):
            self.zstd_lib = importlib.import_module("compression").zstd
            self.zstd_builtin = True
        else:
            try:
                self.zstd_lib = importlib.import_module("zstandard")
                self.zstd_builtin = False
            except ImportError:
                print("Error: zstandard module required when using --zstd flag")
                print("Install with: pip install zstandard")
                sys.exit(1)

    def compress(self, src, dst_path):
        if self.zstd_builtin:
            with src as src_file, self.zstd_lib.open(dst_path, 'wb', level=19) as dst_file:
                shutil.copyfileobj(src_file, dst_file)
        else:
            with src as src_file, open(dst_path, 'wb') as dst_file:
                self.zstd_lib.ZstdCompressor(level=19).copy_stream(src_file, dst_file)

def main():
    logging.basicConfig(format="%(message)s")
    logger = logging.getLogger("copy")
    logger.setLevel(logging.DEBUG)

    args = argparse.ArgumentParser(description="...",
                                   formatter_class=argparse.RawTextHelpFormatter)
    args.add_argument("from_path", metavar="from", help="from")
    args.add_argument("to_path", metavar="to", help="to")
    args.add_argument("--zstd", action="store_true", help="Use Zstandard compression")
    cdc.add_args(args)

    args = args.parse_args()

    from_path = os.path.normpath(args.from_path)
    to_path = os.path.normpath(args.to_path)

    # Import zstd only if compression is requested
    if args.zstd:
        zstd_module = ZstdCompress()
    else:
        zstd_module = None

    if os.path.isfile(from_path):
        tar = tarfile.open(from_path, "r")
    else:
        tar = None

    if tar:
        handle_tar(logger, tar, to_path, args, zstd_module)
    else:
        handle_dir(logger, from_path, to_path, args, zstd_module)

def copy_chunks(logger, f, to_path: str, use_compression: bool, zstd_module, what: str):
    """Write each chunk of an open file as its own blob (as fs2json.py names them)."""
    for c in cdc.chunks(f.read()):
        file_hash = hashlib.sha256(c).hexdigest()
        filename = file_hash[0:HASH_LENGTH] + (".bin.zst" if use_compression else ".bin")
        to_abs = os.path.join(to_path, filename)
        if os.path.exists(to_abs):
            logger.info("Exists, skipped {} (chunk of {})".format(to_abs, what))
        elif use_compression:
            logger.info("Compressing chunk of {} {}".format(what, to_abs))
            zstd_module.compress(io.BytesIO(c), to_abs)
        else:
            logger.info("Chunk of {} {}".format(what, to_abs))
            with open(to_abs, "wb") as out:
                out.write(c)

def handle_dir(logger, from_path: str, to_path: str, args, zstd_module):
    use_compression = args.zstd
    def onerror(oserror):
        logger.warning(oserror)

    files = os.walk(from_path, onerror=onerror)

    for f in files:
        dirpath, dirnames, filenames = f

        for filename in filenames:
            absname = os.path.join(dirpath, filename)
            st = os.lstat(absname)
            mode = st.st_mode

            assert not stat.S_ISDIR(mode)
            if stat.S_ISLNK(mode) or stat.S_ISCHR(mode) or stat.S_ISBLK(mode) or stat.S_ISFIFO(mode) or stat.S_ISSOCK(mode):
                continue

            if cdc.split(args, st.st_size):
                with open(absname, "rb") as f:
                    copy_chunks(logger, f, to_path, use_compression, zstd_module, absname)
                continue

            file_hash = hash_file(absname)
            filename = file_hash[0:HASH_LENGTH] + (".bin.zst" if use_compression else ".bin")
            to_abs = os.path.join(to_path, filename)

            if os.path.exists(to_abs):
                logger.info("Exists, skipped {} ({})".format(to_abs, absname))
            else:
                if use_compression:
                    logger.info("Compressing {} {}".format(absname, to_abs))
                    zstd_module.compress(open(absname, 'rb'), to_abs)
                else:
                    logger.info("cp {} {}".format(absname, to_abs))
                    shutil.copyfile(absname, to_abs)

def handle_tar(logger, tar, to_path: str, args, zstd_module):
    use_compression = args.zstd
    for member in tar.getmembers():
        if member.isfile() or member.islnk():
            f = tar.extractfile(member)
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(0)
            if cdc.split(args, size):
                copy_chunks(logger, f, to_path, use_compression, zstd_module, member.name)
                continue
            file_hash = hash_fileobj(f)
            filename = file_hash[0:HASH_LENGTH] + (".bin.zst" if use_compression else ".bin")
            to_abs = os.path.join(to_path, filename)

            if os.path.exists(to_abs):
                logger.info("Exists, skipped {} ({})".format(to_abs, member.name))
            else:
                if use_compression:
                    logger.info("Extracted and compressing {} ({})".format(to_abs, member.name))
                    f.seek(0)
                    zstd_module.compress(f, to_abs)
                else:
                    logger.info("Extracted {} ({})".format(to_abs, member.name))
                    to_file = open(to_abs, "wb")
                    f.seek(0)
                    shutil.copyfileobj(f, to_file)


if __name__ == "__main__":
    main()
