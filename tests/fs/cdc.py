"""Content-defined chunking for large files (shared by fs2json.py and copy-to-sha256.py).

A gear hash (FastCDC-style, with normalized chunking) picks cut points from the
contents, so an insertion early in a file only changes the chunks around it and
the rest still dedupe. Chunks are 256 KiB to 4 MiB, about 1 MiB on average.
Files under THRESHOLD stay one blob.
"""

import hashlib

MIN = 256 * 1024
AVG = 1024 * 1024
MAX = 4 * 1024 * 1024
THRESHOLD = 2 * AVG

# Stricter before AVG, looser after: sizes cluster around AVG.
MASK_SMALL = (1 << 21) - 1
MASK_LARGE = (1 << 19) - 1
GEAR = [int.from_bytes(hashlib.sha256(bytes([i])).digest()[:8], "little") for i in range(256)]
M64 = (1 << 64) - 1


def cut(data, start):
    """End of the chunk that starts at `start`."""
    n = len(data) - start
    if n <= MIN:
        return len(data)
    end = start + min(n, MAX)
    mid = start + min(n, AVG)
    h = 0
    i = start + MIN
    gear = GEAR
    while i < mid:
        h = ((h << 1) + gear[data[i]]) & M64
        if not h & MASK_SMALL:
            return i + 1
        i += 1
    while i < end:
        h = ((h << 1) + gear[data[i]]) & M64
        if not h & MASK_LARGE:
            return i + 1
        i += 1
    return end


def chunks(data):
    """The chunks of `data` (bytes), as memoryviews."""
    view = memoryview(data)
    start = 0
    while start < len(data):
        end = cut(data, start)
        yield view[start:end]
        start = end


def add_args(parser):
    parser.add_argument("--no-chunks", action="store_true",
                        help="One blob per file, however large (the format older armless reads)")
    parser.add_argument("--chunk-threshold", type=int, default=THRESHOLD, metavar="BYTES",
                        help="Files at least this large are split into chunks (default %d)" % THRESHOLD)


def split(args, size):
    """Whether a file of `size` bytes is chunked."""
    return not args.no_chunks and size >= args.chunk_threshold
