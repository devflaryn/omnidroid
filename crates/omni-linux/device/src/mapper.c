// mapper.omni.so -- omnidroid's vendor gralloc mapper: the stable-C AIMapper v5
// (hardware/interfaces/graphics/mapper/stable-c/include/android/hardware/graphics/mapper/IMapper.h,
// android15-release), loaded by libui's Gralloc5.cpp from /vendor/lib64/hw through
// android_load_sphal_library("mapper.omni.so") and dlsym("AIMapper_loadIMapper").
//
// Design: docs/superpowers/specs/2026-09-27-d2-gralloc-design.md. Built with NDK r28c (build.txt);
// links only libc and liblog, exports only AIMapper_loadIMapper.
//
// ---------------------------------------------------------------------------------------------
// Buffer handle (written by the host allocator, hal/gralloc.rs -- which defines the same table):
//   numFds  = 1: fd 0 = one shared-memory region: a 4096-byte metadata page, then the pixels.
//   numInts = 14:
//      0  magic 0x42474d4f ('OMGB')          7  usage, high 32 bits
//      1  layout version (1)                 8  stride, in pixels
//      2  width                              9  buffer id, low 32 bits
//      3  height                            10  buffer id, high 32 bits
//      4  layer count                       11  pixel bytes, low 32 bits
//      5  pixel format (AIDL PixelFormat;   12  pixel bytes, high 32 bits
//         IMPLEMENTATION_DEFINED is laid
//         out as RGBA_8888)
//      6  usage, low 32 bits                13  offset of the pixels in the region (4096)
//
// Metadata page (offset 0 of the region; the same memory in every process that maps the buffer,
// so a set in one process is seen by all). The allocator zero-fills it and writes only `name`;
// every other field reads as its default while zero, and `magic` is written on the first set.
//      0  u32  magic 0x4d474d4f ('OMGM') once any field was set, else 0
//      4  i32  dataspace                    (default 0, Dataspace::UNKNOWN)
//      8  i32  blend mode                   (default 0, BlendMode::INVALID)
//     12  u32  crop valid                   (0: the crop is the whole buffer, 0,0,w,h)
//     16  i32  crop left, top, right, bottom
//     32  reserved (zero) .. 63
//     64  char name[128], NUL-terminated    (written by the host allocator: the requestor name)
//    192  reserved (zero) .. 255
//    256  u32  SMPTE2086 present
//    260  f32  x10: primaries r.x r.y g.x g.y b.x b.y, white point x y, max luminance, min luminance
//    300  u32  CTA861_3 present
//    304  f32  x2: max content light level, max frame average light level
//    312  reserved (zero) .. 511
//    512  SMPTE2094_10: u32 present, u32 length, then up to 1024 bytes (512 .. 1543)
//   1544  reserved (zero) .. 2047
//   2048  SMPTE2094_40: u32 present, u32 length, then up to 1024 bytes (2048 .. 3079)
//   3080  reserved (zero) .. 4095
// The Rust side writes only `name` at 64.
// ---------------------------------------------------------------------------------------------

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <android/log.h>

#define TAG "omni-mapper"
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, TAG, __VA_ARGS__)

#define EXPORT __attribute__((visibility("default")))

// ---- The ABI, transcribed from IMapper.h, cutils/native_handle.h and android/rect.h ----------

typedef struct native_handle {
    int version;  // sizeof(native_handle_t)
    int numFds;
    int numInts;
    int data[];   // numFds fds, then numInts ints
} native_handle_t;
typedef const native_handle_t* buffer_handle_t;

typedef struct ARect {
    int32_t left;
    int32_t top;
    int32_t right;
    int32_t bottom;
} ARect;

typedef uint32_t AIMapper_Version;  // enum AIMapper_Version : uint32_t
enum { AIMAPPER_VERSION_5 = 5 };

typedef int32_t AIMapper_Error;     // enum AIMapper_Error : int32_t
enum {
    AIMAPPER_ERROR_NONE = 0,
    AIMAPPER_ERROR_BAD_DESCRIPTOR = 1,
    AIMAPPER_ERROR_BAD_BUFFER = 2,
    AIMAPPER_ERROR_BAD_VALUE = 3,
    AIMAPPER_ERROR_NO_RESOURCES = 5,
    AIMAPPER_ERROR_UNSUPPORTED = 7,
};

typedef struct AIMapper_MetadataType {
    const char* name;
    int64_t value;
} AIMapper_MetadataType;

typedef struct AIMapper_MetadataTypeDescription {
    AIMapper_MetadataType metadataType;
    const char* description;
    bool isGettable;
    bool isSettable;
    uint8_t reserved[32];
} AIMapper_MetadataTypeDescription;

typedef void (*AIMapper_DumpBufferCallback)(void* context, AIMapper_MetadataType metadataType,
                                            const void* value, size_t valueSize);
typedef void (*AIMapper_BeginDumpBufferCallback)(void* context);

typedef struct AIMapperV5 {
    AIMapper_Error (*importBuffer)(const native_handle_t* handle, buffer_handle_t* outBufferHandle);
    AIMapper_Error (*freeBuffer)(buffer_handle_t buffer);
    AIMapper_Error (*getTransportSize)(buffer_handle_t buffer, uint32_t* outNumFds,
                                       uint32_t* outNumInts);
    AIMapper_Error (*lock)(buffer_handle_t buffer, uint64_t cpuUsage, ARect accessRegion,
                           int acquireFence, void** outData);
    AIMapper_Error (*unlock)(buffer_handle_t buffer, int* releaseFence);
    AIMapper_Error (*flushLockedBuffer)(buffer_handle_t buffer);
    AIMapper_Error (*rereadLockedBuffer)(buffer_handle_t buffer);
    int32_t (*getMetadata)(buffer_handle_t buffer, AIMapper_MetadataType metadataType,
                           void* destBuffer, size_t destBufferSize);
    int32_t (*getStandardMetadata)(buffer_handle_t buffer, int64_t standardMetadataType,
                                   void* destBuffer, size_t destBufferSize);
    AIMapper_Error (*setMetadata)(buffer_handle_t buffer, AIMapper_MetadataType metadataType,
                                  const void* metadata, size_t metadataSize);
    AIMapper_Error (*setStandardMetadata)(buffer_handle_t buffer, int64_t standardMetadataType,
                                          const void* metadata, size_t metadataSize);
    AIMapper_Error (*listSupportedMetadataTypes)(
            const AIMapper_MetadataTypeDescription** outDescriptionList,
            size_t* outNumberOfDescriptions);
    AIMapper_Error (*dumpBuffer)(buffer_handle_t buffer,
                                 AIMapper_DumpBufferCallback dumpBufferCallback, void* context);
    AIMapper_Error (*dumpAllBuffers)(AIMapper_BeginDumpBufferCallback beginDumpCallback,
                                     AIMapper_DumpBufferCallback dumpBufferCallback,
                                     void* context);
    AIMapper_Error (*getReservedRegion)(buffer_handle_t buffer, void** outReservedRegion,
                                        uint64_t* outReservedSize);
} AIMapperV5;

// C++: struct AIMapper { alignas(alignof(max_align_t)) AIMapper_Version version; AIMapperV5 v5; };
typedef struct AIMapper {
    _Alignas(max_align_t) AIMapper_Version version;
    AIMapperV5 v5;
} AIMapper;

// The layout the C++ declaration has on arm64 (max_align_t is 16-aligned; v5 follows the 4-byte
// version at the next 8-byte boundary; 15 function pointers).
_Static_assert(sizeof(native_handle_t) == 12, "native_handle_t header is 3 ints");
_Static_assert(offsetof(native_handle_t, data) == 12, "native_handle_t data follows the header");
_Static_assert(sizeof(ARect) == 16, "ARect");
_Static_assert(sizeof(AIMapper_MetadataType) == 16, "AIMapper_MetadataType");
_Static_assert(offsetof(AIMapper_MetadataTypeDescription, description) == 16, "description");
_Static_assert(offsetof(AIMapper_MetadataTypeDescription, isGettable) == 24, "isGettable");
_Static_assert(offsetof(AIMapper_MetadataTypeDescription, isSettable) == 25, "isSettable");
_Static_assert(offsetof(AIMapper_MetadataTypeDescription, reserved) == 26, "reserved");
_Static_assert(sizeof(AIMapper_MetadataTypeDescription) == 64, "AIMapper_MetadataTypeDescription");
_Static_assert(sizeof(AIMapperV5) == 15 * sizeof(void*), "AIMapperV5 is 15 entries");
_Static_assert(_Alignof(max_align_t) == 16, "arm64 max_align_t");
_Static_assert(_Alignof(AIMapper) == 16, "AIMapper alignment");
_Static_assert(offsetof(AIMapper, version) == 0, "AIMapper.version");
_Static_assert(offsetof(AIMapper, v5) == 8, "AIMapper.v5");
_Static_assert(sizeof(AIMapper) == 128, "AIMapper size");

// ---- AIDL constants (android.hardware.graphics.common, android15-release) --------------------

enum {  // StandardMetadataType
    SMT_INVALID = 0,
    SMT_BUFFER_ID = 1,
    SMT_NAME = 2,
    SMT_WIDTH = 3,
    SMT_HEIGHT = 4,
    SMT_LAYER_COUNT = 5,
    SMT_PIXEL_FORMAT_REQUESTED = 6,
    SMT_PIXEL_FORMAT_FOURCC = 7,
    SMT_PIXEL_FORMAT_MODIFIER = 8,
    SMT_USAGE = 9,
    SMT_ALLOCATION_SIZE = 10,
    SMT_PROTECTED_CONTENT = 11,
    SMT_COMPRESSION = 12,
    SMT_INTERLACED = 13,
    SMT_CHROMA_SITING = 14,
    SMT_PLANE_LAYOUTS = 15,
    SMT_CROP = 16,
    SMT_DATASPACE = 17,
    SMT_BLEND_MODE = 18,
    SMT_SMPTE2086 = 19,
    SMT_CTA861_3 = 20,
    SMT_SMPTE2094_40 = 21,
    SMT_SMPTE2094_10 = 22,
    SMT_STRIDE = 23,
    SMT_COUNT = 24,
};

enum {  // PixelFormat
    PF_RGBA_8888 = 0x1,
    PF_RGBX_8888 = 0x2,
    PF_RGB_888 = 0x3,
    PF_RGB_565 = 0x4,
    PF_BGRA_8888 = 0x5,
    PF_RGBA_FP16 = 0x16,
    PF_BLOB = 0x21,
    PF_IMPLEMENTATION_DEFINED = 0x22,
    PF_RGBA_1010102 = 0x2B,
    PF_R_8 = 0x38,
};

// PlaneLayoutComponentType
#define PLC_R (INT64_C(1) << 10)
#define PLC_G (INT64_C(1) << 11)
#define PLC_B (INT64_C(1) << 12)
#define PLC_RAW (INT64_C(1) << 20)
#define PLC_A (INT64_C(1) << 30)

#define CPU_USAGE_MASK UINT64_C(0xFF)  // BufferUsage CPU_READ_MASK | CPU_WRITE_MASK

#define STANDARD_METADATA_NAME "android.hardware.graphics.common.StandardMetadataType"
#define COMPRESSION_NAME "android.hardware.graphics.common.Compression"
#define INTERLACED_NAME "android.hardware.graphics.common.Interlaced"
#define CHROMA_SITING_NAME "android.hardware.graphics.common.ChromaSiting"
#define COMPONENT_TYPE_NAME "android.hardware.graphics.common.PlaneLayoutComponentType"
#define LIT_LEN(s) (sizeof(s) - 1)

#define FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))

// ---- Formats (single-plane only; spec decision 4) --------------------------------------------

struct component {
    int64_t type;
    int64_t offset_bits;
    int64_t size_bits;
};

struct format_info {
    int32_t format;
    uint32_t bytes_per_pixel;
    uint32_t fourcc;  // DRM fourcc; 0 for BLOB
    uint32_t num_components;
    struct component components[4];
};

static const struct format_info FORMATS[] = {
    {PF_RGBA_8888, 4, FOURCC('A', 'B', '2', '4'), 4,
     {{PLC_R, 0, 8}, {PLC_G, 8, 8}, {PLC_B, 16, 8}, {PLC_A, 24, 8}}},
    {PF_RGBX_8888, 4, FOURCC('X', 'B', '2', '4'), 3,
     {{PLC_R, 0, 8}, {PLC_G, 8, 8}, {PLC_B, 16, 8}}},
    {PF_BGRA_8888, 4, FOURCC('A', 'R', '2', '4'), 4,
     {{PLC_B, 0, 8}, {PLC_G, 8, 8}, {PLC_R, 16, 8}, {PLC_A, 24, 8}}},
    {PF_RGB_888, 3, FOURCC('B', 'G', '2', '4'), 3,
     {{PLC_R, 0, 8}, {PLC_G, 8, 8}, {PLC_B, 16, 8}}},
    {PF_RGB_565, 2, FOURCC('R', 'G', '1', '6'), 3,
     {{PLC_R, 11, 5}, {PLC_G, 5, 6}, {PLC_B, 0, 5}}},
    {PF_RGBA_FP16, 8, FOURCC('A', 'B', '4', 'H'), 4,
     {{PLC_R, 0, 16}, {PLC_G, 16, 16}, {PLC_B, 32, 16}, {PLC_A, 48, 16}}},
    {PF_RGBA_1010102, 4, FOURCC('A', 'B', '3', '0'), 4,
     {{PLC_R, 0, 10}, {PLC_G, 10, 10}, {PLC_B, 20, 10}, {PLC_A, 30, 2}}},
    // The host allocator lays IMPLEMENTATION_DEFINED out as RGBA_8888; PIXEL_FORMAT_REQUESTED
    // still reports 0x22, the value libui validates against.
    {PF_IMPLEMENTATION_DEFINED, 4, FOURCC('A', 'B', '2', '4'), 4,
     {{PLC_R, 0, 8}, {PLC_G, 8, 8}, {PLC_B, 16, 8}, {PLC_A, 24, 8}}},
    {PF_R_8, 1, FOURCC('R', '8', ' ', ' '), 1, {{PLC_R, 0, 8}}},
    {PF_BLOB, 1, 0, 1, {{PLC_RAW, 0, 8}}},
};

static const struct format_info* find_format(int32_t format) {
    for (size_t i = 0; i < sizeof(FORMATS) / sizeof(FORMATS[0]); i++) {
        if (FORMATS[i].format == format) return &FORMATS[i];
    }
    return NULL;
}

// ---- The handle and the metadata page ---------------------------------------------------------

#define HANDLE_MAGIC 0x42474d4fu  // 'OMGB'
#define HANDLE_LAYOUT_VERSION 1
#define HANDLE_NUM_FDS 1
#define HANDLE_NUM_INTS 14
#define META_MAGIC 0x4d474d4fu    // 'OMGM'
#define META_PAGE_SIZE 4096u
// The region's content generation (D3a): a u64 every writer of the pixels bumps -- this mapper
// when a CPU-write lock ends, the host after the GPU wrote them -- so the host GPU knows when its
// copy of the pixels is stale.
#define CONTENT_GENERATION_AT 4088u
#define CPU_WRITE_MASK UINT64_C(0xF0)  // BufferUsage CPU_WRITE_MASK
#define BLOB_MAX 1024u

enum {  // indices into the ints (data[HANDLE_NUM_FDS + i])
    HI_MAGIC = 0,
    HI_VERSION = 1,
    HI_WIDTH = 2,
    HI_HEIGHT = 3,
    HI_LAYERS = 4,
    HI_FORMAT = 5,
    HI_USAGE_LO = 6,
    HI_USAGE_HI = 7,
    HI_STRIDE = 8,
    HI_ID_LO = 9,
    HI_ID_HI = 10,
    HI_BYTES_LO = 11,
    HI_BYTES_HI = 12,
    HI_PIXEL_OFFSET = 13,
};

struct meta_blob {
    uint32_t present;
    uint32_t length;
    uint8_t data[BLOB_MAX];
};

struct omni_meta {
    uint32_t magic;
    int32_t dataspace;
    int32_t blend_mode;
    uint32_t crop_valid;
    int32_t crop[4];
    uint8_t reserved0[32];
    char name[128];
    uint8_t reserved1[64];
    uint32_t smpte2086_present;
    float smpte2086[10];
    uint32_t cta861_3_present;
    float cta861_3[2];
    uint8_t reserved2[200];
    struct meta_blob smpte2094_10;
    uint8_t reserved3[504];
    struct meta_blob smpte2094_40;
};

_Static_assert(offsetof(struct omni_meta, dataspace) == 4, "meta dataspace");
_Static_assert(offsetof(struct omni_meta, blend_mode) == 8, "meta blend mode");
_Static_assert(offsetof(struct omni_meta, crop_valid) == 12, "meta crop valid");
_Static_assert(offsetof(struct omni_meta, crop) == 16, "meta crop");
_Static_assert(offsetof(struct omni_meta, name) == 64, "meta name is where the allocator writes it");
_Static_assert(offsetof(struct omni_meta, smpte2086_present) == 256, "meta smpte2086");
_Static_assert(offsetof(struct omni_meta, smpte2086) == 260, "meta smpte2086 values");
_Static_assert(offsetof(struct omni_meta, cta861_3_present) == 300, "meta cta861_3");
_Static_assert(offsetof(struct omni_meta, cta861_3) == 304, "meta cta861_3 values");
_Static_assert(offsetof(struct omni_meta, smpte2094_10) == 512, "meta smpte2094_10");
_Static_assert(offsetof(struct omni_meta, smpte2094_40) == 2048, "meta smpte2094_40");
_Static_assert(sizeof(struct omni_meta) <= CONTENT_GENERATION_AT, "meta ends before the content generation");

// ---- The registry of imported buffers ---------------------------------------------------------

struct omni_buffer {
    struct omni_buffer* next;
    native_handle_t* handle;  // the imported (cloned) handle; the registry key
    uint8_t* base;            // the whole region, mapped MAP_SHARED
    size_t map_size;
    const struct format_info* fmt;
    int32_t width, height, layers, format;
    uint32_t stride;
    uint32_t pixel_offset;
    uint64_t usage, id, pixel_bytes;
    int locks;          // guarded by g_lock
    int writing_locks;  // of them, locked for CPU writing; guarded by g_lock
};

#define BUCKETS 64
static struct omni_buffer* g_buckets[BUCKETS];
// Recursive: dumpAllBuffers holds it across callbacks, which may call back into the mapper.
static pthread_mutex_t g_lock = PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP;

static size_t bucket_of(const void* p) { return ((uintptr_t)p >> 4) % BUCKETS; }

// g_lock held.
static struct omni_buffer* find_locked(buffer_handle_t handle) {
    if (handle == NULL) return NULL;
    for (struct omni_buffer* b = g_buckets[bucket_of(handle)]; b != NULL; b = b->next) {
        if (b->handle == handle) return b;
    }
    return NULL;
}

static struct omni_meta* meta_of(const struct omni_buffer* b) { return (struct omni_meta*)b->base; }

static void meta_touch(struct omni_meta* m) {
    if (m->magic != META_MAGIC) m->magic = META_MAGIC;
}

static int32_t handle_int(const native_handle_t* h, int i) { return h->data[HANDLE_NUM_FDS + i]; }

static uint64_t handle_u64(const native_handle_t* h, int lo, int hi) {
    return (uint64_t)(uint32_t)handle_int(h, lo) | ((uint64_t)(uint32_t)handle_int(h, hi) << 32);
}

// Checks the shape and the immutable description of a raw handle (no fd use).
static bool handle_shape_ok(const native_handle_t* h) {
    if (h == NULL) return false;
    if (h->version != (int)sizeof(native_handle_t) || h->numFds != HANDLE_NUM_FDS ||
        h->numInts != HANDLE_NUM_INTS) {
        return false;
    }
    return (uint32_t)handle_int(h, HI_MAGIC) == HANDLE_MAGIC &&
           handle_int(h, HI_VERSION) == HANDLE_LAYOUT_VERSION;
}

// ---- Stable-C metadata encoding (IMapperMetadataTypes.h: MetadataWriter / MetadataReader) ----

struct writer {
    uint8_t* dest;  // NULL once a write did not fit: nothing further is written
    size_t room;
    int64_t desired;
};

static void w_bytes(struct writer* w, const void* p, size_t n) {
    w->desired += (int64_t)n;
    if (n == 0) return;
    if (w->dest != NULL && n <= w->room) {
        memcpy(w->dest, p, n);
        w->dest += n;
        w->room -= n;
    } else {
        w->dest = NULL;
        w->room = 0;
    }
}

static void w_i64(struct writer* w, int64_t v) { w_bytes(w, &v, sizeof v); }
static void w_u64(struct writer* w, uint64_t v) { w_bytes(w, &v, sizeof v); }
static void w_i32(struct writer* w, int32_t v) { w_bytes(w, &v, sizeof v); }
static void w_u32(struct writer* w, uint32_t v) { w_bytes(w, &v, sizeof v); }
static void w_f32(struct writer* w, float v) { w_bytes(w, &v, sizeof v); }

static void w_string(struct writer* w, const char* s, size_t n) {
    w_i64(w, (int64_t)n);
    w_bytes(w, s, n);
}

static void w_header(struct writer* w, int64_t type) {
    w_string(w, STANDARD_METADATA_NAME, LIT_LEN(STANDARD_METADATA_NAME));
    w_i64(w, type);
}

static int32_t w_result(const struct writer* w) {
    return w->desired > INT32_MAX ? -AIMAPPER_ERROR_BAD_VALUE : (int32_t)w->desired;
}

struct reader {
    const uint8_t* src;
    size_t left;
    bool ok;
};

static const void* r_take(struct reader* r, size_t n) {
    if (!r->ok || r->left < n) {
        r->ok = false;
        return NULL;
    }
    const void* p = r->src;
    r->src += n;
    r->left -= n;
    return p;
}

static void r_copy(struct reader* r, void* dst, size_t n) {
    const void* p = r_take(r, n);
    if (p != NULL) memcpy(dst, p, n);
}

static int64_t r_i64(struct reader* r) {
    int64_t v = 0;
    r_copy(r, &v, sizeof v);
    return v;
}

static int32_t r_i32(struct reader* r) {
    int32_t v = 0;
    r_copy(r, &v, sizeof v);
    return v;
}

static float r_f32(struct reader* r) {
    float v = 0;
    r_copy(r, &v, sizeof v);
    return v;
}

static void r_header(struct reader* r, int64_t type) {
    int64_t len = r_i64(r);
    if (len != (int64_t)LIT_LEN(STANDARD_METADATA_NAME)) {
        r->ok = false;
        return;
    }
    const void* name = r_take(r, (size_t)len);
    if (name == NULL || memcmp(name, STANDARD_METADATA_NAME, (size_t)len) != 0) {
        r->ok = false;
        return;
    }
    if (r_i64(r) != type) r->ok = false;
}

// ---- get ---------------------------------------------------------------------------------------

static void w_extendable_none(struct writer* w, const char* name, size_t len) {
    w_string(w, name, len);
    w_i64(w, 0);  // NONE
}

static void w_blob(struct writer* w, int64_t type, const struct meta_blob* blob) {
    uint32_t len = blob->length;
    if (len > BLOB_MAX) len = BLOB_MAX;
    uint8_t copy[BLOB_MAX];
    memcpy(copy, blob->data, len);
    w_header(w, type);
    w_i64(w, (int64_t)len);
    w_bytes(w, copy, len);
}

// g_lock held; `b` is registered.
static int32_t encode_standard(const struct omni_buffer* b, int64_t type, void* dest, size_t size) {
    struct writer w = {(uint8_t*)dest, dest != NULL ? size : 0, 0};
    const struct omni_meta* m = meta_of(b);
    switch (type) {
        case SMT_BUFFER_ID:
            w_header(&w, type);
            w_u64(&w, b->id);
            break;
        case SMT_NAME: {
            char name[sizeof m->name];
            memcpy(name, m->name, sizeof name);
            w_header(&w, type);
            w_string(&w, name, strnlen(name, sizeof name));
            break;
        }
        case SMT_WIDTH:
            w_header(&w, type);
            w_u64(&w, (uint64_t)b->width);
            break;
        case SMT_HEIGHT:
            w_header(&w, type);
            w_u64(&w, (uint64_t)b->height);
            break;
        case SMT_LAYER_COUNT:
            w_header(&w, type);
            w_u64(&w, (uint64_t)b->layers);
            break;
        case SMT_PIXEL_FORMAT_REQUESTED:
            w_header(&w, type);
            w_i32(&w, b->format);
            break;
        case SMT_PIXEL_FORMAT_FOURCC:
            w_header(&w, type);
            w_u32(&w, b->fmt->fourcc);
            break;
        case SMT_PIXEL_FORMAT_MODIFIER:
            w_header(&w, type);
            w_u64(&w, 0);  // DRM_FORMAT_MOD_LINEAR
            break;
        case SMT_USAGE:
            w_header(&w, type);
            w_i64(&w, (int64_t)b->usage);
            break;
        case SMT_ALLOCATION_SIZE:
            w_header(&w, type);
            w_u64(&w, b->pixel_bytes);
            break;
        case SMT_PROTECTED_CONTENT:
            w_header(&w, type);
            w_u64(&w, 0);
            break;
        case SMT_COMPRESSION:
            w_header(&w, type);
            w_extendable_none(&w, COMPRESSION_NAME, LIT_LEN(COMPRESSION_NAME));
            break;
        case SMT_INTERLACED:
            w_header(&w, type);
            w_extendable_none(&w, INTERLACED_NAME, LIT_LEN(INTERLACED_NAME));
            break;
        case SMT_CHROMA_SITING:
            w_header(&w, type);
            w_extendable_none(&w, CHROMA_SITING_NAME, LIT_LEN(CHROMA_SITING_NAME));
            break;
        case SMT_PLANE_LAYOUTS: {
            const struct format_info* f = b->fmt;
            int64_t bpp = f->bytes_per_pixel;
            int64_t stride_bytes = (int64_t)b->stride * bpp;
            w_header(&w, type);
            w_i64(&w, 1);  // one plane
            w_i64(&w, f->num_components);
            for (uint32_t i = 0; i < f->num_components; i++) {
                w_string(&w, COMPONENT_TYPE_NAME, LIT_LEN(COMPONENT_TYPE_NAME));
                w_i64(&w, f->components[i].type);
                w_i64(&w, f->components[i].offset_bits);
                w_i64(&w, f->components[i].size_bits);
            }
            w_i64(&w, 0);                            // offsetInBytes, from the locked pointer
            w_i64(&w, bpp * 8);                      // sampleIncrementInBits
            w_i64(&w, stride_bytes);                 // strideInBytes
            w_i64(&w, b->width);                     // widthInSamples
            w_i64(&w, b->height);                    // heightInSamples
            w_i64(&w, stride_bytes * b->height);     // totalSizeInBytes
            w_i64(&w, 1);                            // horizontalSubsampling
            w_i64(&w, 1);                            // verticalSubsampling
            break;
        }
        case SMT_CROP: {
            int32_t crop[4] = {0, 0, b->width, b->height};
            if (m->magic == META_MAGIC && m->crop_valid) memcpy(crop, m->crop, sizeof crop);
            w_header(&w, type);
            w_i64(&w, 1);
            for (int i = 0; i < 4; i++) w_i32(&w, crop[i]);
            break;
        }
        case SMT_DATASPACE:
            w_header(&w, type);
            w_i32(&w, m->magic == META_MAGIC ? m->dataspace : 0);
            break;
        case SMT_BLEND_MODE:
            w_header(&w, type);
            w_i32(&w, m->magic == META_MAGIC ? m->blend_mode : 0);
            break;
        case SMT_SMPTE2086:
            if (m->magic != META_MAGIC || !m->smpte2086_present) return 0;
            w_header(&w, type);
            for (int i = 0; i < 10; i++) w_f32(&w, m->smpte2086[i]);
            break;
        case SMT_CTA861_3:
            if (m->magic != META_MAGIC || !m->cta861_3_present) return 0;
            w_header(&w, type);
            for (int i = 0; i < 2; i++) w_f32(&w, m->cta861_3[i]);
            break;
        case SMT_SMPTE2094_40:
            if (m->magic != META_MAGIC || !m->smpte2094_40.present) return 0;
            w_blob(&w, type, &m->smpte2094_40);
            break;
        case SMT_SMPTE2094_10:
            if (m->magic != META_MAGIC || !m->smpte2094_10.present) return 0;
            w_blob(&w, type, &m->smpte2094_10);
            break;
        case SMT_STRIDE:
            w_header(&w, type);
            w_u32(&w, b->stride);
            break;
        default:
            return -AIMAPPER_ERROR_UNSUPPORTED;
    }
    return w_result(&w);
}

// ---- set ---------------------------------------------------------------------------------------

static AIMapper_Error set_blob(struct omni_meta* m, struct meta_blob* blob, int64_t type,
                              const void* metadata, size_t size) {
    if (size == 0) {
        blob->present = 0;
        return AIMAPPER_ERROR_NONE;
    }
    struct reader r = {(const uint8_t*)metadata, size, true};
    r_header(&r, type);
    int64_t len = r_i64(&r);
    if (!r.ok || len < 0 || (uint64_t)len > r.left) return AIMAPPER_ERROR_BAD_VALUE;
    if ((uint64_t)len > BLOB_MAX) return AIMAPPER_ERROR_NO_RESOURCES;
    const void* data = r_take(&r, (size_t)len);
    meta_touch(m);
    if (len > 0) memcpy(blob->data, data, (size_t)len);
    blob->length = (uint32_t)len;
    __atomic_store_n(&blob->present, 1u, __ATOMIC_RELEASE);
    return AIMAPPER_ERROR_NONE;
}

// g_lock held; `b` is registered.
static AIMapper_Error decode_standard(struct omni_buffer* b, int64_t type, const void* metadata,
                                      size_t size) {
    struct omni_meta* m = meta_of(b);
    if (metadata == NULL && size != 0) return AIMAPPER_ERROR_BAD_VALUE;
    struct reader r = {(const uint8_t*)metadata, size, true};
    switch (type) {
        case SMT_BUFFER_ID:
        case SMT_NAME:
        case SMT_WIDTH:
        case SMT_HEIGHT:
        case SMT_LAYER_COUNT:
        case SMT_PIXEL_FORMAT_REQUESTED:
        case SMT_USAGE:
            // IMapper.h: BAD_VALUE "when the field is constant and can never be set".
            return AIMAPPER_ERROR_BAD_VALUE;
        case SMT_DATASPACE:
        case SMT_BLEND_MODE: {
            r_header(&r, type);
            int32_t v = r_i32(&r);
            if (!r.ok) return AIMAPPER_ERROR_BAD_VALUE;
            meta_touch(m);
            if (type == SMT_DATASPACE) {
                m->dataspace = v;
            } else {
                m->blend_mode = v;
            }
            return AIMAPPER_ERROR_NONE;
        }
        case SMT_CROP: {
            r_header(&r, type);
            int64_t count = r_i64(&r);
            if (!r.ok || count < 0) return AIMAPPER_ERROR_BAD_VALUE;
            if (count == 0) {
                meta_touch(m);
                m->crop_valid = 0;  // back to the whole buffer
                return AIMAPPER_ERROR_NONE;
            }
            if (count > 1) return AIMAPPER_ERROR_UNSUPPORTED;  // one crop rectangle per buffer
            int32_t crop[4];
            for (int i = 0; i < 4; i++) crop[i] = r_i32(&r);
            if (!r.ok) return AIMAPPER_ERROR_BAD_VALUE;
            meta_touch(m);
            memcpy(m->crop, crop, sizeof crop);
            __atomic_store_n(&m->crop_valid, 1u, __ATOMIC_RELEASE);
            return AIMAPPER_ERROR_NONE;
        }
        case SMT_SMPTE2086: {
            if (size == 0) {
                m->smpte2086_present = 0;
                return AIMAPPER_ERROR_NONE;
            }
            r_header(&r, type);
            float v[10];
            for (int i = 0; i < 10; i++) v[i] = r_f32(&r);
            if (!r.ok) return AIMAPPER_ERROR_BAD_VALUE;
            meta_touch(m);
            memcpy(m->smpte2086, v, sizeof v);
            __atomic_store_n(&m->smpte2086_present, 1u, __ATOMIC_RELEASE);
            return AIMAPPER_ERROR_NONE;
        }
        case SMT_CTA861_3: {
            if (size == 0) {
                m->cta861_3_present = 0;
                return AIMAPPER_ERROR_NONE;
            }
            r_header(&r, type);
            float v[2];
            for (int i = 0; i < 2; i++) v[i] = r_f32(&r);
            if (!r.ok) return AIMAPPER_ERROR_BAD_VALUE;
            meta_touch(m);
            memcpy(m->cta861_3, v, sizeof v);
            __atomic_store_n(&m->cta861_3_present, 1u, __ATOMIC_RELEASE);
            return AIMAPPER_ERROR_NONE;
        }
        case SMT_SMPTE2094_40:
            return set_blob(m, &m->smpte2094_40, type, metadata, size);
        case SMT_SMPTE2094_10:
            return set_blob(m, &m->smpte2094_10, type, metadata, size);
        default:
            return AIMAPPER_ERROR_UNSUPPORTED;
    }
}

// ---- AIMapperV5 --------------------------------------------------------------------------------

static AIMapper_Error omni_importBuffer(const native_handle_t* handle,
                                        buffer_handle_t* outBufferHandle) {
    if (outBufferHandle == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    *outBufferHandle = NULL;
    if (!handle_shape_ok(handle)) {
        LOGE("importBuffer: not an omni buffer handle");
        return AIMAPPER_ERROR_BAD_BUFFER;
    }
    int32_t width = handle_int(handle, HI_WIDTH);
    int32_t height = handle_int(handle, HI_HEIGHT);
    int32_t layers = handle_int(handle, HI_LAYERS);
    int32_t format = handle_int(handle, HI_FORMAT);
    uint32_t stride = (uint32_t)handle_int(handle, HI_STRIDE);
    uint32_t pixel_offset = (uint32_t)handle_int(handle, HI_PIXEL_OFFSET);
    uint64_t pixel_bytes = handle_u64(handle, HI_BYTES_LO, HI_BYTES_HI);
    const struct format_info* fmt = find_format(format);
    if (width <= 0 || height <= 0 || layers <= 0 || fmt == NULL || stride < (uint32_t)width ||
        pixel_offset < META_PAGE_SIZE) {
        LOGE("importBuffer: bad description %dx%d layers %d format %d stride %u offset %u", width,
             height, layers, format, stride, pixel_offset);
        return AIMAPPER_ERROR_BAD_BUFFER;
    }
    uint64_t needed;
    if (__builtin_mul_overflow((uint64_t)stride, (uint64_t)height, &needed) ||
        __builtin_mul_overflow(needed, (uint64_t)fmt->bytes_per_pixel, &needed) ||
        __builtin_mul_overflow(needed, (uint64_t)layers, &needed) || pixel_bytes < needed) {
        LOGE("importBuffer: %llu pixel bytes is short of %ux%d format %d",
             (unsigned long long)pixel_bytes, stride, height, format);
        return AIMAPPER_ERROR_BAD_BUFFER;
    }
    int fd = handle->data[0];
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0) {
        LOGE("importBuffer: fd %d: %s", fd, strerror(errno));
        return AIMAPPER_ERROR_BAD_BUFFER;
    }
    uint64_t end;
    if (st.st_size <= 0 || __builtin_add_overflow((uint64_t)pixel_offset, pixel_bytes, &end) ||
        (uint64_t)st.st_size < end || (uint64_t)st.st_size > SIZE_MAX) {
        LOGE("importBuffer: region of %lld bytes cannot hold %llu pixel bytes at %u",
             (long long)st.st_size, (unsigned long long)pixel_bytes, pixel_offset);
        return AIMAPPER_ERROR_BAD_BUFFER;
    }

    struct omni_buffer* b = calloc(1, sizeof *b);
    native_handle_t* clone =
            malloc(sizeof(native_handle_t) + sizeof(int) * (HANDLE_NUM_FDS + HANDLE_NUM_INTS));
    if (b == NULL || clone == NULL) {
        free(b);
        free(clone);
        return AIMAPPER_ERROR_NO_RESOURCES;
    }
    int dup_fd = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    if (dup_fd < 0) {
        LOGE("importBuffer: dup: %s", strerror(errno));
        free(b);
        free(clone);
        return AIMAPPER_ERROR_NO_RESOURCES;
    }
    size_t map_size = (size_t)st.st_size;
    void* base = mmap(NULL, map_size, PROT_READ | PROT_WRITE, MAP_SHARED, dup_fd, 0);
    if (base == MAP_FAILED) {
        LOGE("importBuffer: mmap %zu bytes: %s", map_size, strerror(errno));
        close(dup_fd);
        free(b);
        free(clone);
        return AIMAPPER_ERROR_NO_RESOURCES;
    }
    clone->version = (int)sizeof(native_handle_t);
    clone->numFds = HANDLE_NUM_FDS;
    clone->numInts = HANDLE_NUM_INTS;
    clone->data[0] = dup_fd;
    memcpy(&clone->data[HANDLE_NUM_FDS], &handle->data[HANDLE_NUM_FDS],
           sizeof(int) * HANDLE_NUM_INTS);

    b->handle = clone;
    b->base = base;
    b->map_size = map_size;
    b->fmt = fmt;
    b->width = width;
    b->height = height;
    b->layers = layers;
    b->format = format;
    b->stride = stride;
    b->pixel_offset = pixel_offset;
    b->usage = handle_u64(handle, HI_USAGE_LO, HI_USAGE_HI);
    b->id = handle_u64(handle, HI_ID_LO, HI_ID_HI);
    b->pixel_bytes = pixel_bytes;

    pthread_mutex_lock(&g_lock);
    size_t k = bucket_of(clone);
    b->next = g_buckets[k];
    g_buckets[k] = b;
    pthread_mutex_unlock(&g_lock);

    *outBufferHandle = clone;
    return AIMAPPER_ERROR_NONE;
}

static AIMapper_Error omni_freeBuffer(buffer_handle_t buffer) {
    if (buffer == NULL) return AIMAPPER_ERROR_BAD_BUFFER;
    pthread_mutex_lock(&g_lock);
    struct omni_buffer** link = &g_buckets[bucket_of(buffer)];
    while (*link != NULL && (*link)->handle != buffer) link = &(*link)->next;
    struct omni_buffer* b = *link;
    if (b != NULL) *link = b->next;
    pthread_mutex_unlock(&g_lock);
    if (b == NULL) return AIMAPPER_ERROR_BAD_BUFFER;
    munmap(b->base, b->map_size);
    close(b->handle->data[0]);
    free(b->handle);
    free(b);
    return AIMAPPER_ERROR_NONE;
}

static AIMapper_Error omni_getTransportSize(buffer_handle_t buffer, uint32_t* outNumFds,
                                            uint32_t* outNumInts) {
    if (outNumFds == NULL || outNumInts == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    pthread_mutex_lock(&g_lock);
    bool ok = find_locked(buffer) != NULL || handle_shape_ok(buffer);
    pthread_mutex_unlock(&g_lock);
    if (!ok) return AIMAPPER_ERROR_BAD_BUFFER;
    *outNumFds = HANDLE_NUM_FDS;
    *outNumInts = HANDLE_NUM_INTS;
    return AIMAPPER_ERROR_NONE;
}

// Waits for and closes a fence the caller handed over; false if it could not be waited on.
static bool wait_fence(int fence) {
    if (fence < 0) return true;
    struct pollfd p = {.fd = fence, .events = POLLIN, .revents = 0};
    int r;
    do {
        r = poll(&p, 1, -1);
    } while (r < 0 && errno == EINTR);
    if (r < 0) LOGE("lock: waiting on acquire fence %d: %s", fence, strerror(errno));
    close(fence);
    return r >= 0;
}

static AIMapper_Error omni_lock(buffer_handle_t buffer, uint64_t cpuUsage, ARect region,
                                int acquireFence, void** outData) {
    bool fence_ok = wait_fence(acquireFence);  // ownership is ours even on error
    if (outData == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    *outData = NULL;
    pthread_mutex_lock(&g_lock);
    struct omni_buffer* b = find_locked(buffer);
    AIMapper_Error err = AIMAPPER_ERROR_NONE;
    if (b == NULL) {
        err = AIMAPPER_ERROR_BAD_BUFFER;
    } else if ((cpuUsage & CPU_USAGE_MASK) == 0) {
        err = AIMAPPER_ERROR_BAD_VALUE;
    } else if (!(region.left == 0 && region.top == 0 && region.right == 0 && region.bottom == 0) &&
               (region.left < 0 || region.top < 0 || region.left > region.right ||
                region.top > region.bottom || region.right > b->width ||
                region.bottom > b->height)) {
        err = AIMAPPER_ERROR_BAD_VALUE;
    } else if (!fence_ok) {
        err = AIMAPPER_ERROR_NO_RESOURCES;
    } else {
        b->locks++;
        if (cpuUsage & CPU_WRITE_MASK) b->writing_locks++;
        *outData = b->base + b->pixel_offset;
    }
    pthread_mutex_unlock(&g_lock);
    return err;
}

static AIMapper_Error omni_unlock(buffer_handle_t buffer, int* releaseFence) {
    if (releaseFence != NULL) *releaseFence = -1;
    pthread_mutex_lock(&g_lock);
    struct omni_buffer* b = find_locked(buffer);
    AIMapper_Error err = AIMAPPER_ERROR_NONE;
    if (b == NULL || b->locks <= 0) {
        err = AIMAPPER_ERROR_BAD_BUFFER;
    } else {
        b->locks--;
        // The pixels may have changed: the host GPU's copy of them is stale.
        if (b->writing_locks > 0) {
            b->writing_locks--;
            __atomic_fetch_add((uint64_t*)(void*)(b->base + CONTENT_GENERATION_AT), 1, __ATOMIC_SEQ_CST);
        }
    }
    pthread_mutex_unlock(&g_lock);
    return err;
}

// The region is coherent shared memory: nothing to flush or reread.
static AIMapper_Error omni_flushLockedBuffer(buffer_handle_t buffer) {
    pthread_mutex_lock(&g_lock);
    bool ok = find_locked(buffer) != NULL;
    pthread_mutex_unlock(&g_lock);
    return ok ? AIMAPPER_ERROR_NONE : AIMAPPER_ERROR_BAD_BUFFER;
}

static AIMapper_Error omni_rereadLockedBuffer(buffer_handle_t buffer) {
    return omni_flushLockedBuffer(buffer);
}

static int32_t omni_getStandardMetadata(buffer_handle_t buffer, int64_t type, void* destBuffer,
                                        size_t destBufferSize) {
    pthread_mutex_lock(&g_lock);
    struct omni_buffer* b = find_locked(buffer);
    int32_t ret = b == NULL ? -AIMAPPER_ERROR_BAD_BUFFER
                            : encode_standard(b, type, destBuffer, destBufferSize);
    pthread_mutex_unlock(&g_lock);
    return ret;
}

static bool is_standard(AIMapper_MetadataType t) {
    return t.name != NULL && strcmp(t.name, STANDARD_METADATA_NAME) == 0;
}

static int32_t omni_getMetadata(buffer_handle_t buffer, AIMapper_MetadataType type,
                                void* destBuffer, size_t destBufferSize) {
    if (!is_standard(type)) return -AIMAPPER_ERROR_UNSUPPORTED;
    return omni_getStandardMetadata(buffer, type.value, destBuffer, destBufferSize);
}

static AIMapper_Error omni_setStandardMetadata(buffer_handle_t buffer, int64_t type,
                                               const void* metadata, size_t metadataSize) {
    pthread_mutex_lock(&g_lock);
    struct omni_buffer* b = find_locked(buffer);
    AIMapper_Error ret = b == NULL ? AIMAPPER_ERROR_BAD_BUFFER
                                   : decode_standard(b, type, metadata, metadataSize);
    pthread_mutex_unlock(&g_lock);
    return ret;
}

static AIMapper_Error omni_setMetadata(buffer_handle_t buffer, AIMapper_MetadataType type,
                                       const void* metadata, size_t metadataSize) {
    if (!is_standard(type)) return AIMAPPER_ERROR_UNSUPPORTED;
    return omni_setStandardMetadata(buffer, type.value, metadata, metadataSize);
}

#define DESCRIBE(type, text, settable) \
    {{STANDARD_METADATA_NAME, (type)}, (text), true, (settable), {0}}

static const AIMapper_MetadataTypeDescription DESCRIPTIONS[] = {
    DESCRIBE(SMT_BUFFER_ID, "BUFFER_ID", false),
    DESCRIBE(SMT_NAME, "NAME", false),
    DESCRIBE(SMT_WIDTH, "WIDTH", false),
    DESCRIBE(SMT_HEIGHT, "HEIGHT", false),
    DESCRIBE(SMT_LAYER_COUNT, "LAYER_COUNT", false),
    DESCRIBE(SMT_PIXEL_FORMAT_REQUESTED, "PIXEL_FORMAT_REQUESTED", false),
    DESCRIBE(SMT_PIXEL_FORMAT_FOURCC, "PIXEL_FORMAT_FOURCC", false),
    DESCRIBE(SMT_PIXEL_FORMAT_MODIFIER, "PIXEL_FORMAT_MODIFIER", false),
    DESCRIBE(SMT_USAGE, "USAGE", false),
    DESCRIBE(SMT_ALLOCATION_SIZE, "ALLOCATION_SIZE", false),
    DESCRIBE(SMT_PROTECTED_CONTENT, "PROTECTED_CONTENT", false),
    DESCRIBE(SMT_COMPRESSION, "COMPRESSION", false),
    DESCRIBE(SMT_INTERLACED, "INTERLACED", false),
    DESCRIBE(SMT_CHROMA_SITING, "CHROMA_SITING", false),
    DESCRIBE(SMT_PLANE_LAYOUTS, "PLANE_LAYOUTS", false),
    DESCRIBE(SMT_CROP, "CROP", true),
    DESCRIBE(SMT_DATASPACE, "DATASPACE", true),
    DESCRIBE(SMT_BLEND_MODE, "BLEND_MODE", true),
    DESCRIBE(SMT_SMPTE2086, "SMPTE2086", true),
    DESCRIBE(SMT_CTA861_3, "CTA861_3", true),
    DESCRIBE(SMT_SMPTE2094_40, "SMPTE2094_40", true),
    DESCRIBE(SMT_SMPTE2094_10, "SMPTE2094_10", true),
    DESCRIBE(SMT_STRIDE, "STRIDE", false),
};
_Static_assert(sizeof(DESCRIPTIONS) / sizeof(DESCRIPTIONS[0]) == SMT_COUNT - 1,
               "every StandardMetadataType but INVALID");

static AIMapper_Error omni_listSupportedMetadataTypes(
        const AIMapper_MetadataTypeDescription** outDescriptionList,
        size_t* outNumberOfDescriptions) {
    if (outDescriptionList == NULL || outNumberOfDescriptions == NULL) {
        return AIMAPPER_ERROR_UNSUPPORTED;
    }
    *outDescriptionList = DESCRIPTIONS;
    *outNumberOfDescriptions = sizeof(DESCRIPTIONS) / sizeof(DESCRIPTIONS[0]);
    return AIMAPPER_ERROR_NONE;
}

// g_lock held; `b` is registered.
static AIMapper_Error dump_locked(const struct omni_buffer* b, AIMapper_DumpBufferCallback cb,
                                  void* context) {
    uint8_t stack[2048];
    for (int64_t type = SMT_BUFFER_ID; type < SMT_COUNT; type++) {
        int32_t size = encode_standard(b, type, NULL, 0);
        if (size < 0) return AIMAPPER_ERROR_NO_RESOURCES;
        if (size == 0) continue;  // an optional value that is absent
        uint8_t* value = (size_t)size <= sizeof stack ? stack : malloc((size_t)size);
        if (value == NULL) return AIMAPPER_ERROR_NO_RESOURCES;
        int32_t written = encode_standard(b, type, value, (size_t)size);
        if (written > 0 && written <= size) {
            AIMapper_MetadataType t = {STANDARD_METADATA_NAME, type};
            cb(context, t, value, (size_t)written);
        }
        if (value != stack) free(value);
    }
    return AIMAPPER_ERROR_NONE;
}

static AIMapper_Error omni_dumpBuffer(buffer_handle_t buffer, AIMapper_DumpBufferCallback cb,
                                      void* context) {
    if (cb == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    pthread_mutex_lock(&g_lock);
    struct omni_buffer* b = find_locked(buffer);
    AIMapper_Error ret = b == NULL ? AIMAPPER_ERROR_BAD_BUFFER : dump_locked(b, cb, context);
    pthread_mutex_unlock(&g_lock);
    return ret;
}

static AIMapper_Error omni_dumpAllBuffers(AIMapper_BeginDumpBufferCallback begin,
                                          AIMapper_DumpBufferCallback cb, void* context) {
    if (begin == NULL || cb == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    AIMapper_Error ret = AIMAPPER_ERROR_NONE;
    pthread_mutex_lock(&g_lock);
    for (size_t k = 0; k < BUCKETS && ret == AIMAPPER_ERROR_NONE; k++) {
        for (struct omni_buffer* b = g_buckets[k]; b != NULL; b = b->next) {
            begin(context);
            ret = dump_locked(b, cb, context);
            if (ret != AIMAPPER_ERROR_NONE) break;
        }
    }
    pthread_mutex_unlock(&g_lock);
    return ret;
}

static AIMapper_Error omni_getReservedRegion(buffer_handle_t buffer, void** outReservedRegion,
                                             uint64_t* outReservedSize) {
    if (outReservedRegion == NULL || outReservedSize == NULL) return AIMAPPER_ERROR_BAD_VALUE;
    *outReservedRegion = NULL;
    *outReservedSize = 0;
    pthread_mutex_lock(&g_lock);
    bool ok = find_locked(buffer) != NULL;
    pthread_mutex_unlock(&g_lock);
    return ok ? AIMAPPER_ERROR_NONE : AIMAPPER_ERROR_BAD_BUFFER;
}

static AIMapper g_mapper = {
    .version = AIMAPPER_VERSION_5,
    .v5 =
            {
                    .importBuffer = omni_importBuffer,
                    .freeBuffer = omni_freeBuffer,
                    .getTransportSize = omni_getTransportSize,
                    .lock = omni_lock,
                    .unlock = omni_unlock,
                    .flushLockedBuffer = omni_flushLockedBuffer,
                    .rereadLockedBuffer = omni_rereadLockedBuffer,
                    .getMetadata = omni_getMetadata,
                    .getStandardMetadata = omni_getStandardMetadata,
                    .setMetadata = omni_setMetadata,
                    .setStandardMetadata = omni_setStandardMetadata,
                    .listSupportedMetadataTypes = omni_listSupportedMetadataTypes,
                    .dumpBuffer = omni_dumpBuffer,
                    .dumpAllBuffers = omni_dumpAllBuffers,
                    .getReservedRegion = omni_getReservedRegion,
            },
};

EXPORT AIMapper_Error AIMapper_loadIMapper(AIMapper** outImplementation) {
    if (outImplementation == NULL) return AIMAPPER_ERROR_UNSUPPORTED;
    *outImplementation = &g_mapper;
    return AIMAPPER_ERROR_NONE;
}
