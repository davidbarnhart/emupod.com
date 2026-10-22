/* A decoder-shaped integer workload for an ARM7TDMI (ARMv4T, no libc): a bit reader, table lookups (CRC32),
   fixed-point multiply-accumulate with 64-bit products (smull, as an MP3 decoder's synthesis filter), and block
   copies. Ends in done(), an endless loop the harness stops at. */
typedef unsigned int u32; typedef int s32; typedef long long s64; typedef unsigned char u8;
static u32 crc_table[256]; static s32 coef[512]; static s32 pcm[1152 * 2]; static u8 stream[8192]; static s32 out[1152 * 2];
volatile u32 result;
void done(void) __attribute__((noinline, naked)); void done(void) { __asm__ volatile("1: b 1b"); }
static void init(void) {
    for (u32 i = 0; i < 256; i++) { u32 c = i; for (int k = 0; k < 8; k++) c = c & 1 ? 0xedb88320u ^ (c >> 1) : c >> 1; crc_table[i] = c; }
    u32 x = 12345; for (int i = 0; i < 512; i++) { x = x * 1103515245u + 12345u; coef[i] = (s32)(x >> 8) - (1 << 23); }
    for (int i = 0; i < 8192; i++) { x = x * 1103515245u + 12345u; stream[i] = x >> 24; }
}
static u32 crc32(const u8 *p, int n, u32 c) { c = ~c; while (n--) c = crc_table[(c ^ *p++) & 0xff] ^ (c >> 8); return ~c; }
static u32 getbits(const u8 *p, u32 *pos, int n) { u32 v = 0; while (n--) { v = (v << 1) | ((p[*pos >> 3] >> (7 - (*pos & 7))) & 1); (*pos)++; } return v; }
static void frame(int f) {
    u32 pos = (f * 977) & 0x7fff;                                   /* unpack "samples" from the bit stream */
    for (int i = 0; i < 1152 * 2; i++) pcm[i] = ((s32)getbits(stream, &pos, 12) - 2048) << 12, pos &= 0xffff;
    for (int i = 0; i < 1152 * 2; i += 32) {                       /* the synthesis filter: 32 x 16 MACs a block */
        for (int j = 0; j < 32; j++) {
            s64 acc = 0; const s32 *c = &coef[(j * 16) & 511];
            for (int k = 0; k < 16; k++) acc += (s64)pcm[i + ((j + k) & 31)] * c[k];
            out[i + j] = (s32)(acc >> 24);
        }
    }
    s32 *d = pcm, *s = out; for (int i = 0; i < 1152 * 2; i++) *d++ = *s++;   /* copy out */
}
void main_(void) {
    init(); u32 c = 0;
    for (int f = 0; f < 40; f++) { frame(f); c = crc32((const u8 *)pcm, sizeof pcm, c); }
    result = c; done();
}
__attribute__((naked, section(".text.start"))) void _start(void) { __asm__ volatile("ldr sp, =0x00100000\n bl main_\n 1: b 1b"); }
