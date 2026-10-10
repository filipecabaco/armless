// Prepare an arm64 Linux kernel for armless_boot_linux, which expects a raw
// `Image`. Distribution kernels are usually compressed:
//
//   * gzip (Alpine/Debian/Ubuntu `vmlinuz` on arm64 is Image.gz)
//   * EFI zboot (`MZ` + "zimg" header with a gzip or zstd payload)

const IMAGE_MAGIC_OFFSET = 0x38;
const IMAGE_MAGIC = 0x644D5241; // "ARM\x64"

function u32(bytes, offset)
{
    return (bytes[offset] | bytes[offset + 1] << 8 | bytes[offset + 2] << 16 | bytes[offset + 3] << 24) >>> 0;
}

/* global DecompressionStream */
async function gunzip(bytes)
{
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
    return new Uint8Array(await new Response(stream).arrayBuffer());
}

/** Frame_Content_Size of the first zstd frame, or -1 if absent. */
function zstd_content_size(bytes)
{
    if(u32(bytes, 0) !== 0xFD2FB528) return -1;
    const fhd = bytes[4];
    const fcs_flag = fhd >> 6;
    const single_segment = fhd >> 5 & 1;
    const dict_id_flag = fhd & 3;
    let pos = 5 + (single_segment ? 0 : 1) + [0, 1, 2, 4][dict_id_flag];
    if(fcs_flag === 0)
    {
        return single_segment ? bytes[pos] : -1;
    }
    if(fcs_flag === 1) return (bytes[pos] | bytes[pos + 1] << 8) + 256;
    if(fcs_flag === 2) return u32(bytes, pos);
    return u32(bytes, pos) + u32(bytes, pos + 4) * 0x100000000;
}

/**
 * @param {ArrayBuffer} buffer
 * @param {function(number, Uint8Array): ArrayBuffer} zstd_decompress
 * @return {Promise<ArrayBuffer>}
 */
export async function prepare_arm64_kernel(buffer, zstd_decompress)
{
    let bytes = new Uint8Array(buffer);

    if(bytes[0] === 0x1F && bytes[1] === 0x8B)
    {
        bytes = await gunzip(bytes);
    }
    else if(bytes[0] === 0x4D && bytes[1] === 0x5A && u32(bytes, 4) === 0x676D697A) // "MZ" .. "zimg"
    {
        const offset = u32(bytes, 8);
        const size = u32(bytes, 12);
        const type = new TextDecoder().decode(bytes.subarray(24, 56)).replace(/\0.*$/s, "");
        const payload = bytes.subarray(offset, offset + size);
        if(type === "gzip")
        {
            bytes = await gunzip(payload);
        }
        else if(type === "zstd" || type === "zstd22")
        {
            const n = zstd_content_size(payload);
            if(n < 0) throw new Error("armless: zboot zstd payload without a content size");
            bytes = new Uint8Array(zstd_decompress(n, payload));
        }
        else
        {
            throw new Error("armless: unsupported zboot compression: " + type);
        }
    }

    if(bytes.length < 64 || u32(bytes, IMAGE_MAGIC_OFFSET) !== IMAGE_MAGIC)
    {
        throw new Error("armless: kernel is not an arm64 Image (after decompression)");
    }
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length);
}
