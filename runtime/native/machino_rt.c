/*
 * machino native C runtime (v1)
 *
 * Heap objects are allocated with malloc and never moved. There is no cycle-
 * collecting GC in v1 — mno_gc_collect() is a no-op. Programs that form
 * reference cycles among heap objects will leak until process exit.
 */

#include "machino_rt.h"

#include <arpa/inet.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <math.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

const char *mno_fail_msg = NULL;

static int g_argc = 0;
static char **g_argv = NULL;

/* ---- object tags (low 3 bits of meta word 0) ---- */
enum {
    TAG_BYTES = 0,
    TAG_ARR = 1,
    TAG_STRUCT = 3,
    TAG_CLOSURE = 4,
    TAG_ENUM = 5,
};

typedef struct {
    mno_i64 meta;
    mno_i64 word1;
} MnoHeader;

static mno_i64 mno_meta(mno_i64 tag, mno_i64 count) {
    return (count << 4) | (tag & 7);
}

static mno_i64 mno_tag_of(const MnoHeader *h) { return h->meta & 7; }

static mno_i64 mno_count_of(const MnoHeader *h) { return h->meta >> 4; }

static MnoHeader *mno_hdr(mno_i64 p) {
    if (p == 0) {
        mno_fail("runtime error: null pointer dereference");
    }
    return (MnoHeader *)(intptr_t)p;
}

static mno_i64 mno_addr(void *p) { return (mno_i64)(intptr_t)p; }

static void *mno_xmalloc(size_t n) {
    void *p = malloc(n);
    if (!p) {
        mno_fail("runtime error: out of memory");
    }
    return p;
}

static void *mno_xrealloc(void *p, size_t n) {
    void *q = realloc(p, n);
    if (!q) {
        mno_fail("runtime error: out of memory");
    }
    return q;
}

/* Registered heap object addresses (so float bit-patterns are never treated as
 * pointers during opportunistic string/pointer checks). */
static uintptr_t *g_heap_keys = NULL;
static size_t g_heap_cap = 0;
static size_t g_heap_len = 0;

static size_t mno_heap_hash(uintptr_t key) {
    /* SplitMix64-ish mix; good enough for pointer keys. */
    key ^= key >> 30;
    key *= 0xbf58476d1ce4e5b9ULL;
    key ^= key >> 27;
    key *= 0x94d049bb133111ebULL;
    key ^= key >> 31;
    return (size_t)key;
}

static void mno_heap_rehash(size_t new_cap) {
    uintptr_t *next = (uintptr_t *)calloc(new_cap, sizeof(uintptr_t));
    if (!next) {
        mno_fail("runtime error: out of memory");
    }
    for (size_t i = 0; i < g_heap_cap; i++) {
        uintptr_t k = g_heap_keys[i];
        if (k == 0) {
            continue;
        }
        size_t j = mno_heap_hash(k) & (new_cap - 1);
        while (next[j] != 0) {
            j = (j + 1) & (new_cap - 1);
        }
        next[j] = k;
    }
    free(g_heap_keys);
    g_heap_keys = next;
    g_heap_cap = new_cap;
}

static void mno_heap_register(void *p) {
    if (!p) {
        return;
    }
    if (g_heap_cap == 0) {
        mno_heap_rehash(64);
    } else if (g_heap_len * 2 >= g_heap_cap) {
        mno_heap_rehash(g_heap_cap * 2);
    }
    uintptr_t key = (uintptr_t)p;
    size_t i = mno_heap_hash(key) & (g_heap_cap - 1);
    while (g_heap_keys[i] != 0) {
        if (g_heap_keys[i] == key) {
            return;
        }
        i = (i + 1) & (g_heap_cap - 1);
    }
    g_heap_keys[i] = key;
    g_heap_len++;
}

static int mno_heap_contains(mno_i64 v) {
    if (v == 0 || g_heap_cap == 0) {
        return 0;
    }
    uintptr_t key = (uintptr_t)(intptr_t)v;
    size_t i = mno_heap_hash(key) & (g_heap_cap - 1);
    for (;;) {
        uintptr_t k = g_heap_keys[i];
        if (k == 0) {
            return 0;
        }
        if (k == key) {
            return 1;
        }
        i = (i + 1) & (g_heap_cap - 1);
    }
}

/* ---- fail / init ---- */

void mno_fail(const char *msg) {
    mno_fail_msg = msg;
    if (msg) {
        fprintf(stderr, "%s\n", msg);
    }
    exit(1);
}

void mno_init(int argc, char **argv) {
    g_argc = argc;
    g_argv = argv;
}

/* ---- checked integer arithmetic ---- */

static void mno_overflow(void) { mno_fail("runtime error: integer overflow"); }

mno_i64 mno_iadd(mno_i64 a, mno_i64 b) {
    mno_i64 r;
    if (__builtin_add_overflow(a, b, &r)) {
        mno_overflow();
    }
    return r;
}

mno_i64 mno_isub(mno_i64 a, mno_i64 b) {
    mno_i64 r;
    if (__builtin_sub_overflow(a, b, &r)) {
        mno_overflow();
    }
    return r;
}

mno_i64 mno_imul(mno_i64 a, mno_i64 b) {
    mno_i64 r;
    if (__builtin_mul_overflow(a, b, &r)) {
        mno_overflow();
    }
    return r;
}

mno_i64 mno_idiv(mno_i64 a, mno_i64 b) {
    if (b == 0) {
        mno_fail("runtime error: division by zero");
    }
    if (a == INT64_MIN && b == -1) {
        mno_overflow();
    }
    return a / b;
}

mno_i64 mno_irem(mno_i64 a, mno_i64 b) {
    if (b == 0) {
        mno_fail("runtime error: division by zero");
    }
    if (a == INT64_MIN && b == -1) {
        mno_overflow();
    }
    return a % b;
}

/* ---- print ---- */

void mno_print_i64(mno_i64 v) {
    printf("%" PRId64 "\n", v);
}

void mno_print_f64(mno_f64 v) {
    if (isfinite(v) && v == trunc(v)) {
        printf("%.1f\n", v);
    } else {
        printf("%g\n", v);
    }
}

void mno_print_bool(mno_i64 v) {
    printf("%s\n", v ? "true" : "false");
}

void mno_print_str(mno_i64 s) {
    MnoHeader *h = mno_hdr(s);
    if (mno_tag_of(h) != TAG_BYTES) {
        mno_fail("runtime error: expected string");
    }
    const char *bytes = (const char *)(h + 1);
    mno_i64 len = mno_count_of(h);
    fwrite(bytes, 1, (size_t)len, stdout);
    putchar('\n');
}

/* ---- strings ---- */

static mno_i64 mno_str_from_bytes(const void *bytes, size_t len) {
    size_t size = sizeof(MnoHeader) + len;
    MnoHeader *h = (MnoHeader *)mno_xmalloc(size);
    h->meta = mno_meta(TAG_BYTES, (mno_i64)len);
    h->word1 = 0;
    if (len > 0) {
        memcpy(h + 1, bytes, len);
    }
    mno_heap_register(h);
    return mno_addr(h);
}

mno_i64 mno_str_from_lit(const char *bytes, mno_i64 len) {
    if (len < 0) {
        mno_fail("runtime error: negative string length");
    }
    return mno_str_from_bytes(bytes, (size_t)len);
}

static const char *mno_str_bytes(mno_i64 s, mno_i64 *out_len) {
    MnoHeader *h = mno_hdr(s);
    if (mno_tag_of(h) != TAG_BYTES) {
        mno_fail("runtime error: expected string");
    }
    if (out_len) {
        *out_len = mno_count_of(h);
    }
    return (const char *)(h + 1);
}

mno_i64 mno_str_len(mno_i64 s) { return mno_count_of(mno_hdr(s)); }

mno_i64 mno_str_at(mno_i64 s, mno_i64 i) {
    mno_i64 len;
    const char *bytes = mno_str_bytes(s, &len);
    if (i < 0 || i >= len) {
        mno_fail("runtime error: char_at out of bounds");
    }
    return (unsigned char)bytes[i];
}

mno_i64 mno_str_concat(mno_i64 a, mno_i64 b) {
    mno_i64 la, lb;
    const char *ba = mno_str_bytes(a, &la);
    const char *bb = mno_str_bytes(b, &lb);
    size_t total = (size_t)la + (size_t)lb;
    char *buf = (char *)mno_xmalloc(total);
    memcpy(buf, ba, (size_t)la);
    memcpy(buf + la, bb, (size_t)lb);
    mno_i64 out = mno_str_from_bytes(buf, total);
    free(buf);
    return out;
}

mno_i64 mno_str_eq(mno_i64 a, mno_i64 b) {
    mno_i64 la, lb;
    const char *ba = mno_str_bytes(a, &la);
    const char *bb = mno_str_bytes(b, &lb);
    if (la != lb) {
        return 0;
    }
    return memcmp(ba, bb, (size_t)la) == 0 ? 1 : 0;
}

mno_i64 mno_substr(mno_i64 s, mno_i64 start, mno_i64 end) {
    mno_i64 len;
    const char *bytes = mno_str_bytes(s, &len);
    if (start < 0 || start > end || end > len) {
        mno_fail("runtime error: substr out of range");
    }
    return mno_str_from_bytes(bytes + start, (size_t)(end - start));
}

mno_i64 mno_chr(mno_i64 c) {
    if (c < 0 || c > 255) {
        mno_fail("runtime error: chr byte value out of range 0..=255");
    }
    unsigned char b = (unsigned char)c;
    return mno_str_from_bytes(&b, 1);
}

/* ---- UTF-8 helpers ---- */

typedef struct {
    uint32_t *cps;
    size_t len;
} MnoCpView;

static int mno_utf8_decode(const char *s, size_t n, size_t i, uint32_t *out_cp, size_t *out_adv) {
    if (i >= n) {
        return 0;
    }
    unsigned char c0 = (unsigned char)s[i];
    if (c0 < 0x80) {
        *out_cp = c0;
        *out_adv = 1;
        return 1;
    }
    if ((c0 & 0xE0) == 0xC0 && i + 1 < n) {
        unsigned char c1 = (unsigned char)s[i + 1];
        if ((c1 & 0xC0) == 0x80) {
            *out_cp = ((uint32_t)(c0 & 0x1F) << 6) | (c1 & 0x3F);
            if (*out_cp >= 0x80) {
                *out_adv = 2;
                return 1;
            }
        }
    } else if ((c0 & 0xF0) == 0xE0 && i + 2 < n) {
        unsigned char c1 = (unsigned char)s[i + 1];
        unsigned char c2 = (unsigned char)s[i + 2];
        if ((c1 & 0xC0) == 0x80 && (c2 & 0xC0) == 0x80) {
            *out_cp = ((uint32_t)(c0 & 0x0F) << 12) | ((uint32_t)(c1 & 0x3F) << 6) | (c2 & 0x3F);
            if (*out_cp >= 0x800 && !(*out_cp >= 0xD800 && *out_cp <= 0xDFFF)) {
                *out_adv = 3;
                return 1;
            }
        }
    } else if ((c0 & 0xF8) == 0xF0 && i + 3 < n) {
        unsigned char c1 = (unsigned char)s[i + 1];
        unsigned char c2 = (unsigned char)s[i + 2];
        unsigned char c3 = (unsigned char)s[i + 3];
        if ((c1 & 0xC0) == 0x80 && (c2 & 0xC0) == 0x80 && (c3 & 0xC0) == 0x80) {
            *out_cp = ((uint32_t)(c0 & 0x07) << 18) | ((uint32_t)(c1 & 0x3F) << 12) |
                      ((uint32_t)(c2 & 0x3F) << 6) | (c3 & 0x3F);
            if (*out_cp >= 0x10000 && *out_cp <= 0x10FFFF) {
                *out_adv = 4;
                return 1;
            }
        }
    }
    return -1;
}

static MnoCpView mno_cp_view(mno_i64 s) {
    mno_i64 len;
    const char *bytes = mno_str_bytes(s, &len);
    MnoCpView view;
    view.cps = NULL;
    view.len = 0;
    if (len == 0) {
        return view;
    }
    size_t cap = 8;
    view.cps = (uint32_t *)mno_xmalloc(cap * sizeof(uint32_t));
    for (size_t i = 0; i < (size_t)len;) {
        uint32_t cp;
        size_t adv;
        int ok = mno_utf8_decode(bytes, (size_t)len, i, &cp, &adv);
        if (ok <= 0) {
            free(view.cps);
            mno_fail("runtime error: invalid UTF-8 in string");
        }
        if (view.len == cap) {
            cap *= 2;
            view.cps = (uint32_t *)mno_xrealloc(view.cps, cap * sizeof(uint32_t));
        }
        view.cps[view.len++] = cp;
        i += adv;
    }
    return view;
}

static void mno_cp_view_free(MnoCpView *view) {
    free(view->cps);
    view->cps = NULL;
    view->len = 0;
}

static size_t mno_utf8_encode(uint32_t cp, char out[4]) {
    if (cp <= 0x7F) {
        out[0] = (char)cp;
        return 1;
    }
    if (cp <= 0x7FF) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp <= 0xFFFF) {
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18));
    out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
    out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

mno_i64 mno_len_cp(mno_i64 s) {
    MnoCpView view = mno_cp_view(s);
    mno_i64 n = (mno_i64)view.len;
    mno_cp_view_free(&view);
    return n;
}

mno_i64 mno_char_at_cp(mno_i64 s, mno_i64 i) {
    MnoCpView view = mno_cp_view(s);
    if (i < 0 || (size_t)i >= view.len) {
        mno_cp_view_free(&view);
        mno_fail("runtime error: codepoint index out of bounds");
    }
    mno_i64 cp = (mno_i64)view.cps[(size_t)i];
    mno_cp_view_free(&view);
    return cp;
}

mno_i64 mno_substr_cp(mno_i64 s, mno_i64 start, mno_i64 end) {
    MnoCpView view = mno_cp_view(s);
    if (start < 0 || start > end || end > (mno_i64)view.len) {
        mno_cp_view_free(&view);
        mno_fail("runtime error: codepoint index out of bounds");
    }
    char *buf = NULL;
    size_t cap = 0;
    size_t used = 0;
    for (mno_i64 i = start; i < end; i++) {
        char tmp[4];
        size_t n = mno_utf8_encode(view.cps[(size_t)i], tmp);
        if (used + n > cap) {
            cap = cap ? cap * 2 : 16;
            buf = (char *)mno_xrealloc(buf, cap);
        }
        memcpy(buf + used, tmp, n);
        used += n;
    }
    mno_i64 out = mno_str_from_bytes(buf, used);
    free(buf);
    mno_cp_view_free(&view);
    return out;
}

mno_i64 mno_chr_cp(mno_i64 cp) {
    if (cp < 0 || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) {
        mno_fail("runtime error: invalid Unicode scalar value");
    }
    char tmp[4];
    size_t n = mno_utf8_encode((uint32_t)cp, tmp);
    return mno_str_from_bytes(tmp, n);
}

/* ---- arrays ---- */

static mno_i64 *mno_arr_slots(MnoHeader *h) { return (mno_i64 *)(h + 1); }

static MnoHeader *mno_arr_hdr(mno_i64 a) {
    MnoHeader *h = mno_hdr(a);
    if (mno_tag_of(h) != TAG_ARR) {
        mno_fail("runtime error: expected array");
    }
    return h;
}

mno_i64 mno_arr_new(mno_i64 len) {
    if (len < 0) {
        mno_fail("runtime error: negative array length");
    }
    size_t size = sizeof(MnoHeader) + (size_t)len * sizeof(mno_i64);
    MnoHeader *h = (MnoHeader *)mno_xmalloc(size);
    h->meta = mno_meta(TAG_ARR, len);
    h->word1 = 0;
    mno_i64 *slots = mno_arr_slots(h);
    for (mno_i64 i = 0; i < len; i++) {
        slots[i] = 0;
    }
    mno_heap_register(h);
    return mno_addr(h);
}

mno_i64 mno_arr_len(mno_i64 a) { return mno_count_of(mno_arr_hdr(a)); }

static void mno_arr_bounds(MnoHeader *h, mno_i64 i) {
    mno_i64 len = mno_count_of(h);
    if (i < 0 || i >= len) {
        mno_fail("runtime error: array index out of bounds");
    }
}

mno_i64 mno_arr_get(mno_i64 a, mno_i64 i) {
    MnoHeader *h = mno_arr_hdr(a);
    mno_arr_bounds(h, i);
    return mno_arr_slots(h)[i];
}

void mno_arr_set(mno_i64 a, mno_i64 i, mno_i64 v) {
    MnoHeader *h = mno_arr_hdr(a);
    mno_arr_bounds(h, i);
    mno_arr_slots(h)[i] = v;
}

mno_i64 mno_arr_push(mno_i64 a, mno_i64 v) {
    MnoHeader *h = mno_arr_hdr(a);
    mno_i64 len = mno_count_of(h);
    mno_i64 new_len = mno_iadd(len, 1);
    size_t size = sizeof(MnoHeader) + (size_t)new_len * sizeof(mno_i64);
    MnoHeader *nh = (MnoHeader *)mno_xmalloc(size);
    nh->meta = mno_meta(TAG_ARR, new_len);
    nh->word1 = 0;
    mno_i64 *dst = mno_arr_slots(nh);
    mno_i64 *src = mno_arr_slots(h);
    for (mno_i64 i = 0; i < len; i++) {
        dst[i] = src[i];
    }
    dst[len] = v;
    mno_heap_register(nh);
    return mno_addr(nh);
}

/* ---- structs ---- */

static MnoHeader *mno_struct_hdr(mno_i64 s) {
    MnoHeader *h = mno_hdr(s);
    if (mno_tag_of(h) != TAG_STRUCT) {
        mno_fail("runtime error: expected struct");
    }
    return h;
}

static mno_i64 *mno_struct_fields(MnoHeader *h) { return (mno_i64 *)(h + 1); }

mno_i64 mno_struct_new(mno_i64 nfields) {
    if (nfields < 0) {
        mno_fail("runtime error: negative struct field count");
    }
    size_t size = sizeof(MnoHeader) + (size_t)nfields * sizeof(mno_i64);
    MnoHeader *h = (MnoHeader *)mno_xmalloc(size);
    h->meta = mno_meta(TAG_STRUCT, nfields);
    h->word1 = 0;
    memset(mno_struct_fields(h), 0, (size_t)nfields * sizeof(mno_i64));
    mno_heap_register(h);
    return mno_addr(h);
}

mno_i64 mno_struct_get(mno_i64 s, mno_i64 idx) {
    MnoHeader *h = mno_struct_hdr(s);
    if (idx < 0 || idx >= mno_count_of(h)) {
        mno_fail("runtime error: struct field index out of bounds");
    }
    return mno_struct_fields(h)[idx];
}

void mno_struct_set(mno_i64 s, mno_i64 idx, mno_i64 v) {
    MnoHeader *h = mno_struct_hdr(s);
    if (idx < 0 || idx >= mno_count_of(h)) {
        mno_fail("runtime error: struct field index out of bounds");
    }
    mno_struct_fields(h)[idx] = v;
}

/* ---- enums ---- */

typedef struct {
    MnoHeader hdr;
    mno_i64 tag;
    /* payloads follow */
} MnoEnumHead;

static MnoEnumHead *mno_enum_ptr(mno_i64 e) {
    MnoEnumHead *h = (MnoEnumHead *)mno_hdr(e);
    if (mno_tag_of(&h->hdr) != TAG_ENUM) {
        mno_fail("runtime error: expected enum");
    }
    return h;
}

static mno_i64 *mno_enum_payloads(MnoEnumHead *h) {
    return (mno_i64 *)(h + 1);
}

mno_i64 mno_enum_new(mno_i64 tag, mno_i64 npayloads) {
    if (npayloads < 0) {
        mno_fail("runtime error: negative enum payload count");
    }
    size_t size = sizeof(MnoEnumHead) + (size_t)npayloads * sizeof(mno_i64);
    MnoEnumHead *h = (MnoEnumHead *)mno_xmalloc(size);
    h->hdr.meta = mno_meta(TAG_ENUM, npayloads);
    h->hdr.word1 = 0;
    h->tag = tag;
    memset(mno_enum_payloads(h), 0, (size_t)npayloads * sizeof(mno_i64));
    mno_heap_register(h);
    return mno_addr(h);
}

mno_i64 mno_enum_tag(mno_i64 e) { return mno_enum_ptr(e)->tag; }

static void mno_enum_bounds(MnoEnumHead *h, mno_i64 i) {
    if (i < 0 || i >= mno_count_of(&h->hdr)) {
        mno_fail("runtime error: enum payload index out of bounds");
    }
}

mno_i64 mno_enum_payload(mno_i64 e, mno_i64 i) {
    MnoEnumHead *h = mno_enum_ptr(e);
    mno_enum_bounds(h, i);
    return mno_enum_payloads(h)[i];
}

void mno_enum_set_payload(mno_i64 e, mno_i64 i, mno_i64 v) {
    MnoEnumHead *h = mno_enum_ptr(e);
    mno_enum_bounds(h, i);
    mno_enum_payloads(h)[i] = v;
}

/* ---- closures ---- */

static MnoHeader *mno_closure_hdr(mno_i64 c) {
    MnoHeader *h = mno_hdr(c);
    if (mno_tag_of(h) != TAG_CLOSURE) {
        mno_fail("runtime error: expected closure");
    }
    return h;
}

static mno_i64 *mno_closure_slots(MnoHeader *h) { return (mno_i64 *)(h + 1); }

static mno_i64 mno_closure_ncaptures(MnoHeader *h) {
    mno_i64 nslots = mno_count_of(h);
    if (nslots < 1) {
        mno_fail("runtime error: malformed closure");
    }
    return nslots - 1;
}

static void mno_closure_capture_bounds(MnoHeader *h, mno_i64 i) {
    if (i < 0 || i >= mno_closure_ncaptures(h)) {
        mno_fail("runtime error: closure capture index out of bounds");
    }
}

mno_i64 mno_closure_new(void *fn, mno_i64 ncaptures) {
    if (ncaptures < 0) {
        mno_fail("runtime error: negative closure capture count");
    }
    if (!fn) {
        mno_fail("runtime error: null closure function pointer");
    }
    mno_i64 nslots = mno_iadd(ncaptures, 1);
    size_t size = sizeof(MnoHeader) + (size_t)nslots * sizeof(mno_i64);
    MnoHeader *h = (MnoHeader *)mno_xmalloc(size);
    h->meta = mno_meta(TAG_CLOSURE, nslots);
    h->word1 = 0;
    mno_i64 *slots = mno_closure_slots(h);
    slots[0] = (mno_i64)(intptr_t)fn;
    for (mno_i64 i = 1; i < nslots; i++) {
        slots[i] = 0;
    }
    mno_heap_register(h);
    return mno_addr(h);
}

void mno_closure_set(mno_i64 c, mno_i64 i, mno_i64 v) {
    MnoHeader *h = mno_closure_hdr(c);
    mno_closure_capture_bounds(h, i);
    mno_closure_slots(h)[i + 1] = v;
}

mno_i64 mno_closure_get(mno_i64 c, mno_i64 i) {
    MnoHeader *h = mno_closure_hdr(c);
    mno_closure_capture_bounds(h, i);
    return mno_closure_slots(h)[i + 1];
}

void *mno_closure_fn(mno_i64 c) {
    MnoHeader *h = mno_closure_hdr(c);
    return (void *)(intptr_t)mno_closure_slots(h)[0];
}

/* ---- value clone ---- */

static int mno_is_str(mno_i64 v) {
    if (!mno_heap_contains(v)) {
        return 0;
    }
    MnoHeader *h = (MnoHeader *)(intptr_t)v;
    return mno_tag_of(h) == TAG_BYTES;
}

static mno_i64 mno_str_clone(mno_i64 s) {
    mno_i64 len;
    const char *bytes = mno_str_bytes(s, &len);
    return mno_str_from_bytes(bytes, (size_t)len);
}

static mno_i64 mno_clone_pointer_value(mno_i64 v) {
    if (v == 0) {
        return 0;
    }
    MnoHeader *h = (MnoHeader *)(intptr_t)v;
    mno_i64 tag = mno_tag_of(h);
    if (tag == TAG_BYTES) {
        return mno_str_clone(v);
    }
    if (tag == TAG_ARR) {
        mno_i64 len = mno_count_of(h);
        mno_i64 out = mno_arr_new(len);
        mno_i64 *src = mno_arr_slots(h);
        for (mno_i64 i = 0; i < len; i++) {
            mno_i64 elem = src[i];
            if (mno_is_str(elem)) {
                elem = mno_str_clone(elem);
            }
            mno_arr_set(out, i, elem);
        }
        return out;
    }
    return v;
}

mno_i64 mno_value_clone(mno_i64 v, char kind) {
    switch (kind) {
    case 'i':
    case 'b':
    case 'f':
        return v;
    case 's':
        if (v == 0) {
            return 0;
        }
        return mno_str_clone(v);
    case 'p':
        return mno_clone_pointer_value(v);
    default:
        mno_fail("runtime error: invalid value_clone kind");
    }
}

/* ---- tasks (pthreads) ---- */

typedef struct {
    pthread_t thread;
    int in_use;
    int joined;
    int spawn_ok;
    char ret_kind;
    mno_i64 result;
    mno_i64 *argv;
    mno_i64 argc;
    mno_task_fn fn;
} MnoTaskSlot;

static MnoTaskSlot *g_tasks = NULL;
static size_t g_task_cap = 0;

static mno_i64 mno_task_alloc(void) {
    for (size_t i = 1; i < g_task_cap; i++) {
        if (!g_tasks[i].in_use) {
            memset(&g_tasks[i], 0, sizeof(MnoTaskSlot));
            g_tasks[i].in_use = 1;
            return (mno_i64)i;
        }
    }
    size_t new_cap = g_task_cap ? g_task_cap * 2 : 16;
    g_tasks = (MnoTaskSlot *)mno_xrealloc(g_tasks, new_cap * sizeof(MnoTaskSlot));
    for (size_t i = g_task_cap; i < new_cap; i++) {
        memset(&g_tasks[i], 0, sizeof(MnoTaskSlot));
    }
    g_task_cap = new_cap;
    return mno_task_alloc();
}

static MnoTaskSlot *mno_task_slot(mno_i64 h, const char *op) {
    if (h <= 0 || (size_t)h >= g_task_cap || !g_tasks[h].in_use) {
        char msg[256];
        snprintf(msg, sizeof(msg), "%s: invalid task handle %" PRId64, op, h);
        mno_fail(msg);
    }
    return &g_tasks[h];
}

static mno_i64 *mno_task_argv_copy(
    mno_i64 *argv,
    mno_i64 argc,
    const char *arg_kinds
) {
    if (argc < 0) {
        mno_fail("runtime error: negative task argc");
    }
    if (argc == 0) {
        return NULL;
    }
    if (!argv || !arg_kinds) {
        mno_fail("runtime error: missing task argv/kinds");
    }
    mno_i64 *copy = (mno_i64 *)mno_xmalloc((size_t)argc * sizeof(mno_i64));
    for (mno_i64 i = 0; i < argc; i++) {
        copy[i] = mno_value_clone(argv[i], arg_kinds[i]);
    }
    return copy;
}

static void *mno_task_entry(void *arg) {
    MnoTaskSlot *slot = (MnoTaskSlot *)arg;
    slot->result = slot->fn(slot->argv);
    free(slot->argv);
    slot->argv = NULL;
    return NULL;
}

mno_i64 mno_task_spawn(
    mno_task_fn fn,
    mno_i64 *argv,
    mno_i64 argc,
    char ret_kind,
    const char *arg_kinds
) {
    if (!fn) {
        mno_fail("runtime error: null task function pointer");
    }
    if (ret_kind != 'i' && ret_kind != 'b' && ret_kind != 'f' && ret_kind != 's') {
        mno_fail("runtime error: invalid task return kind");
    }
    if (argc > 0 && !arg_kinds) {
        mno_fail("runtime error: missing task arg kinds");
    }
    mno_i64 h = mno_task_alloc();
    MnoTaskSlot *slot = &g_tasks[h];
    slot->fn = fn;
    slot->argc = argc;
    slot->ret_kind = ret_kind;
    slot->argv = mno_task_argv_copy(argv, argc, arg_kinds);
    slot->spawn_ok = (pthread_create(&slot->thread, NULL, mno_task_entry, slot) == 0);
    if (!slot->spawn_ok) {
        free(slot->argv);
        slot->argv = NULL;
        slot->in_use = 0;
        mno_fail("runtime error: task spawn failed");
    }
    return h;
}

static mno_i64 mno_task_do_join(mno_i64 h, const char *op) {
    MnoTaskSlot *slot = mno_task_slot(h, op);
    if (slot->joined) {
        mno_fail("runtime error: task already joined");
    }
    if (!slot->spawn_ok) {
        mno_fail("runtime error: task spawn failed");
    }
    int rc = pthread_join(slot->thread, NULL);
    if (rc != 0) {
        slot->in_use = 0;
        mno_fail("runtime error: task join failed");
    }
    slot->joined = 1;
    return slot->result;
}

mno_i64 mno_task_join_i64(mno_i64 h) {
    MnoTaskSlot *slot = mno_task_slot(h, "task_join_i64");
    mno_i64 out = mno_task_do_join(h, "task_join_i64");
    if (slot->ret_kind != 'i' && slot->ret_kind != 'b') {
        slot->in_use = 0;
        mno_fail("runtime error: task join kind mismatch");
    }
    slot->in_use = 0;
    return out;
}

mno_f64 mno_task_join_f64(mno_i64 h) {
    MnoTaskSlot *slot = mno_task_slot(h, "task_join_f64");
    mno_i64 bits = mno_task_do_join(h, "task_join_f64");
    if (slot->ret_kind != 'f') {
        slot->in_use = 0;
        mno_fail("runtime error: task join kind mismatch");
    }
    slot->in_use = 0;
    return mno_bits_to_f64(bits);
}

mno_i64 mno_task_join_str(mno_i64 h) {
    MnoTaskSlot *slot = mno_task_slot(h, "task_join_str");
    mno_i64 raw = mno_task_do_join(h, "task_join_str");
    if (slot->ret_kind != 's') {
        slot->in_use = 0;
        mno_fail("runtime error: task join kind mismatch");
    }
    mno_i64 out = raw ? mno_str_clone(raw) : 0;
    slot->in_use = 0;
    return out;
}

/* ---- channels ---- */

enum {
    CHAN_VAL_I64 = 1,
    CHAN_VAL_F64 = 2,
    CHAN_VAL_STR = 3,
};

typedef struct MnoChanNode {
    char kind;
    mno_i64 payload;
    struct MnoChanNode *next;
} MnoChanNode;

typedef struct {
    int in_use;
    int closed;
    pthread_mutex_t mu;
    pthread_cond_t not_empty;
    MnoChanNode *head;
    MnoChanNode *tail;
} MnoChanSlot;

static MnoChanSlot *g_chans = NULL;
static size_t g_chan_cap = 0;

static mno_i64 mno_chan_alloc(void) {
    for (size_t i = 1; i < g_chan_cap; i++) {
        if (!g_chans[i].in_use) {
            MnoChanSlot *slot = &g_chans[i];
            memset(slot, 0, sizeof(MnoChanSlot));
            if (pthread_mutex_init(&slot->mu, NULL) != 0 ||
                pthread_cond_init(&slot->not_empty, NULL) != 0) {
                mno_fail("runtime error: channel init failed");
            }
            slot->in_use = 1;
            slot->closed = 0;
            slot->head = NULL;
            slot->tail = NULL;
            return (mno_i64)i;
        }
    }
    size_t new_cap = g_chan_cap ? g_chan_cap * 2 : 16;
    g_chans = (MnoChanSlot *)mno_xrealloc(g_chans, new_cap * sizeof(MnoChanSlot));
    for (size_t i = g_chan_cap; i < new_cap; i++) {
        memset(&g_chans[i], 0, sizeof(MnoChanSlot));
    }
    g_chan_cap = new_cap;
    return mno_chan_alloc();
}

static MnoChanSlot *mno_chan_slot(mno_i64 id, const char *op) {
    if (id <= 0 || (size_t)id >= g_chan_cap || !g_chans[id].in_use) {
        char msg[256];
        snprintf(msg, sizeof(msg), "%s: invalid channel id %" PRId64, op, id);
        mno_fail(msg);
    }
    return &g_chans[id];
}

static void mno_chan_fail_closed(const char *op) {
    char msg[256];
    snprintf(msg, sizeof(msg), "%s: channel closed", op);
    mno_fail(msg);
}

static void mno_chan_fail_recv_closed(const char *op) {
    char msg[256];
    snprintf(msg, sizeof(msg), "%s: channel closed and empty", op);
    mno_fail(msg);
}

static void mno_chan_enqueue(MnoChanSlot *slot, char kind, mno_i64 payload) {
    MnoChanNode *node = (MnoChanNode *)mno_xmalloc(sizeof(MnoChanNode));
    node->kind = kind;
    node->payload = payload;
    node->next = NULL;
    if (slot->tail) {
        slot->tail->next = node;
    } else {
        slot->head = node;
    }
    slot->tail = node;
    pthread_cond_signal(&slot->not_empty);
}

static MnoChanNode *mno_chan_dequeue(MnoChanSlot *slot) {
    MnoChanNode *node = slot->head;
    if (!node) {
        return NULL;
    }
    slot->head = node->next;
    if (!slot->head) {
        slot->tail = NULL;
    }
    node->next = NULL;
    return node;
}

mno_i64 mno_chan_new(void) { return mno_chan_alloc(); }

void mno_chan_close(mno_i64 id) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_close");
    pthread_mutex_lock(&slot->mu);
    slot->closed = 1;
    pthread_cond_broadcast(&slot->not_empty);
    pthread_mutex_unlock(&slot->mu);
}

static void mno_chan_send_locked(MnoChanSlot *slot, char kind, mno_i64 payload, const char *op) {
    if (slot->closed) {
        pthread_mutex_unlock(&slot->mu);
        mno_chan_fail_closed(op);
    }
    mno_chan_enqueue(slot, kind, payload);
    pthread_mutex_unlock(&slot->mu);
}

void mno_chan_send_i64(mno_i64 id, mno_i64 v) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_send_i64");
    pthread_mutex_lock(&slot->mu);
    mno_chan_send_locked(slot, CHAN_VAL_I64, v, "chan_send_i64");
}

void mno_chan_send_f64(mno_i64 id, mno_f64 v) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_send_f64");
    pthread_mutex_lock(&slot->mu);
    mno_chan_send_locked(slot, CHAN_VAL_F64, mno_f64_to_bits(v), "chan_send_f64");
}

void mno_chan_send_str(mno_i64 id, mno_i64 s) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_send_str");
    mno_i64 owned = s ? mno_str_clone(s) : 0;
    pthread_mutex_lock(&slot->mu);
    mno_chan_send_locked(slot, CHAN_VAL_STR, owned, "chan_send_str");
}

static MnoChanNode *mno_chan_recv_wait(MnoChanSlot *slot, const char *op) {
    while (!slot->head) {
        if (slot->closed) {
            pthread_mutex_unlock(&slot->mu);
            mno_chan_fail_recv_closed(op);
        }
        pthread_cond_wait(&slot->not_empty, &slot->mu);
    }
    return mno_chan_dequeue(slot);
}

mno_i64 mno_chan_recv_i64(mno_i64 id) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_recv_i64");
    pthread_mutex_lock(&slot->mu);
    MnoChanNode *node = mno_chan_recv_wait(slot, "chan_recv_i64");
    if (node->kind != CHAN_VAL_I64) {
        free(node);
        pthread_mutex_unlock(&slot->mu);
        mno_fail("runtime error: channel receive kind mismatch");
    }
    mno_i64 out = node->payload;
    free(node);
    pthread_mutex_unlock(&slot->mu);
    return out;
}

mno_f64 mno_chan_recv_f64(mno_i64 id) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_recv_f64");
    pthread_mutex_lock(&slot->mu);
    MnoChanNode *node = mno_chan_recv_wait(slot, "chan_recv_f64");
    if (node->kind != CHAN_VAL_F64) {
        free(node);
        pthread_mutex_unlock(&slot->mu);
        mno_fail("runtime error: channel receive kind mismatch");
    }
    mno_f64 out = mno_bits_to_f64(node->payload);
    free(node);
    pthread_mutex_unlock(&slot->mu);
    return out;
}

mno_i64 mno_chan_recv_str(mno_i64 id) {
    MnoChanSlot *slot = mno_chan_slot(id, "chan_recv_str");
    pthread_mutex_lock(&slot->mu);
    MnoChanNode *node = mno_chan_recv_wait(slot, "chan_recv_str");
    if (node->kind != CHAN_VAL_STR) {
        free(node);
        pthread_mutex_unlock(&slot->mu);
        mno_fail("runtime error: channel receive kind mismatch");
    }
    mno_i64 out = node->payload;
    free(node);
    pthread_mutex_unlock(&slot->mu);
    return out;
}

/* ---- float ---- */

mno_i64 mno_f64_to_bits(mno_f64 v) {
    mno_i64 bits;
    memcpy(&bits, &v, sizeof(bits));
    return bits;
}

mno_f64 mno_bits_to_f64(mno_i64 bits) {
    mno_f64 v;
    memcpy(&v, &bits, sizeof(v));
    return v;
}

mno_f64 mno_fadd(mno_f64 a, mno_f64 b) { return a + b; }

mno_f64 mno_fsub(mno_f64 a, mno_f64 b) { return a - b; }

mno_f64 mno_fmul(mno_f64 a, mno_f64 b) { return a * b; }

mno_f64 mno_fdiv(mno_f64 a, mno_f64 b) { return a / b; }

mno_i64 mno_to_int(mno_f64 v) {
    if (!isfinite(v) || v < (mno_f64)INT64_MIN || v >= (mno_f64)INT64_MAX) {
        mno_fail("runtime error: to_int: value is out of int range");
    }
    return (mno_i64)trunc(v);
}

mno_f64 mno_to_float(mno_i64 v) { return (mno_f64)v; }

mno_i64 mno_hash_str(mno_i64 s) {
    mno_i64 len;
    const unsigned char *bytes = (const unsigned char *)mno_str_bytes(s, &len);
    uint64_t h = 1469598103934665603ULL;
    for (mno_i64 i = 0; i < len; i++) {
        h ^= bytes[i];
        h *= 1099511628211ULL;
    }
    return (mno_i64)h;
}

void mno_gc_collect(void) {}

/* ---- host helpers ---- */

static char *mno_cstr_from_str(mno_i64 s) {
    mno_i64 len;
    const char *bytes = mno_str_bytes(s, &len);
    char *out = (char *)mno_xmalloc((size_t)len + 1);
    memcpy(out, bytes, (size_t)len);
    out[len] = '\0';
    return out;
}

mno_i64 mno_args(void) {
    int start = 1;
    int n = g_argc > start ? g_argc - start : 0;
    mno_i64 arr = mno_arr_new(n);
    for (int i = 0; i < n; i++) {
        size_t len = strlen(g_argv[start + i]);
        mno_i64 s = mno_str_from_bytes(g_argv[start + i], len);
        mno_arr_set(arr, i, s);
    }
    return arr;
}

mno_i64 mno_getenv(mno_i64 name) {
    char *key = mno_cstr_from_str(name);
    const char *val = getenv(key);
    free(key);
    if (!val) {
        val = "";
    }
    return mno_str_from_bytes(val, strlen(val));
}

mno_i64 mno_clock_ms(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts) != 0) {
        return 0;
    }
    return (mno_i64)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

void mno_sleep_ms(mno_i64 ms) {
    if (ms <= 0) {
        return;
    }
    struct timespec req;
    req.tv_sec = (time_t)(ms / 1000);
    req.tv_nsec = (long)(ms % 1000) * 1000000L;
    while (nanosleep(&req, &req) != 0 && errno == EINTR) {
    }
}

static mno_i64 mno_read_all(FILE *f, size_t initial) {
    size_t cap = initial > 0 ? initial : 4096;
    size_t len = 0;
    char *buf = (char *)mno_xmalloc(cap);
    for (;;) {
        size_t room = cap - len;
        if (room == 0) {
            cap *= 2;
            buf = (char *)mno_xrealloc(buf, cap);
            room = cap - len;
        }
        size_t n = fread(buf + len, 1, room, f);
        len += n;
        if (n < room) {
            break;
        }
    }
    mno_i64 out = mno_str_from_bytes(buf, len);
    free(buf);
    return out;
}

mno_i64 mno_read_file(mno_i64 path) {
    char *p = mno_cstr_from_str(path);
    FILE *f = fopen(p, "rb");
    if (!f) {
        char msg[512];
        snprintf(msg, sizeof(msg), "read_file: cannot read '%s': %s", p, strerror(errno));
        free(p);
        mno_fail(msg);
    }
    mno_i64 out = mno_read_all(f, 0);
    fclose(f);
    free(p);
    return out;
}

mno_i64 mno_write_file(mno_i64 path, mno_i64 data) {
    char *p = mno_cstr_from_str(path);
    mno_i64 len;
    const char *bytes = mno_str_bytes(data, &len);
    FILE *f = fopen(p, "wb");
    if (!f) {
        free(p);
        return 0;
    }
    size_t n = fwrite(bytes, 1, (size_t)len, f);
    int ok = (n == (size_t)len) && fclose(f) == 0;
    if (!ok && f) {
        fclose(f);
    }
    free(p);
    return ok ? 1 : 0;
}

mno_i64 mno_file_exists(mno_i64 path) {
    char *p = mno_cstr_from_str(path);
    int ok = access(p, F_OK) == 0;
    free(p);
    return ok ? 1 : 0;
}

mno_i64 mno_read_line(void) {
    char *line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        return mno_str_from_bytes("", 0);
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
        n--;
    }
    mno_i64 out = mno_str_from_bytes(line, (size_t)n);
    free(line);
    return out;
}

void mno_exit(mno_i64 code) { exit((int)code); }

static void mno_shell_escape(const char *in, char *out, size_t out_cap) {
    size_t j = 0;
    out[j++] = '\'';
    for (size_t i = 0; in[i] != '\0'; i++) {
        if (in[i] == '\'') {
            if (j + 4 >= out_cap) {
                break;
            }
            out[j++] = '\'';
            out[j++] = '\\';
            out[j++] = '\'';
            out[j++] = '\'';
        } else {
            if (j + 2 >= out_cap) {
                break;
            }
            out[j++] = in[i];
        }
    }
    if (j + 1 < out_cap) {
        out[j++] = '\'';
    }
    out[j < out_cap ? j : out_cap - 1] = '\0';
}

mno_i64 mno_http_get(mno_i64 url) {
    char *u = mno_cstr_from_str(url);
    char escaped[8192];
    mno_shell_escape(u, escaped, sizeof(escaped));
    char cmd[8704];
    snprintf(cmd, sizeof(cmd), "curl -sfL --max-time 30 %s 2>/dev/null", escaped);
    FILE *p = popen(cmd, "r");
    free(u);
    if (!p) {
        return mno_str_from_bytes("", 0);
    }
    mno_i64 body = mno_read_all(p, 4096);
    pclose(p);
    return body;
}

/* ---- TCP (POSIX sockets) ---- */

typedef struct {
    int fd;
    int is_listener;
    int in_use;
} MnoTcpSlot;

static MnoTcpSlot *g_tcp = NULL;
static size_t g_tcp_cap = 0;

static mno_i64 mno_tcp_alloc(int fd, int is_listener) {
    for (size_t i = 1; i < g_tcp_cap; i++) {
        if (!g_tcp[i].in_use) {
            g_tcp[i].fd = fd;
            g_tcp[i].is_listener = is_listener;
            g_tcp[i].in_use = 1;
            return (mno_i64)i;
        }
    }
    size_t new_cap = g_tcp_cap ? g_tcp_cap * 2 : 16;
    g_tcp = (MnoTcpSlot *)mno_xrealloc(g_tcp, new_cap * sizeof(MnoTcpSlot));
    for (size_t i = g_tcp_cap; i < new_cap; i++) {
        g_tcp[i].in_use = 0;
        g_tcp[i].fd = -1;
        g_tcp[i].is_listener = 0;
    }
    g_tcp_cap = new_cap;
    return mno_tcp_alloc(fd, is_listener);
}

static MnoTcpSlot *mno_tcp_slot(mno_i64 handle, int want_listener, const char *bad_msg) {
    if (handle <= 0 || (size_t)handle >= g_tcp_cap || !g_tcp[handle].in_use) {
        char msg[256];
        snprintf(msg, sizeof(msg), "%s: invalid handle %" PRId64, bad_msg, handle);
        mno_fail(msg);
    }
    if (want_listener >= 0 && g_tcp[handle].is_listener != want_listener) {
        char msg[256];
        snprintf(msg, sizeof(msg), "%s: invalid handle %" PRId64, bad_msg, handle);
        mno_fail(msg);
    }
    return &g_tcp[handle];
}

mno_i64 mno_tcp_listen(mno_i64 port) {
    if (port < 0 || port > 65535) {
        char msg[128];
        snprintf(msg, sizeof(msg), "tcp_listen: cannot bind port %" PRId64 ": invalid port", port);
        mno_fail(msg);
    }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        char msg[128];
        snprintf(msg, sizeof(msg), "tcp_listen: cannot bind port %" PRId64 ": %s", port,
                 strerror(errno));
        mno_fail(msg);
    }
    int yes = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes));
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons((uint16_t)port);
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        char msg[128];
        snprintf(msg, sizeof(msg), "tcp_listen: cannot bind port %" PRId64 ": %s", port,
                 strerror(errno));
        close(fd);
        mno_fail(msg);
    }
    if (listen(fd, 128) != 0) {
        char msg[128];
        snprintf(msg, sizeof(msg), "tcp_listen: cannot bind port %" PRId64 ": %s", port,
                 strerror(errno));
        close(fd);
        mno_fail(msg);
    }
    return mno_tcp_alloc(fd, 1);
}

mno_i64 mno_tcp_accept(mno_i64 listener) {
    MnoTcpSlot *slot = mno_tcp_slot(listener, 1, "tcp_accept");
    struct sockaddr_in peer;
    socklen_t plen = sizeof(peer);
    int cfd = accept(slot->fd, (struct sockaddr *)&peer, &plen);
    if (cfd < 0) {
        char msg[256];
        snprintf(msg, sizeof(msg), "tcp_accept: %s", strerror(errno));
        mno_fail(msg);
    }
    return mno_tcp_alloc(cfd, 0);
}

mno_i64 mno_tcp_read(mno_i64 conn) {
    MnoTcpSlot *slot = mno_tcp_slot(conn, 0, "tcp_read");
    char buf[65536];
    ssize_t n = recv(slot->fd, buf, sizeof(buf), 0);
    if (n < 0) {
        char msg[256];
        snprintf(msg, sizeof(msg), "tcp_read: %s", strerror(errno));
        mno_fail(msg);
    }
    if (n == 0) {
        return mno_str_from_bytes("", 0);
    }
    return mno_str_from_bytes(buf, (size_t)n);
}

mno_i64 mno_tcp_write(mno_i64 conn, mno_i64 data) {
    MnoTcpSlot *slot = mno_tcp_slot(conn, 0, "tcp_write");
    mno_i64 len;
    const char *bytes = mno_str_bytes(data, &len);
    size_t off = 0;
    while (off < (size_t)len) {
        ssize_t n = send(slot->fd, bytes + off, (size_t)len - off, 0);
        if (n < 0) {
            char msg[256];
            snprintf(msg, sizeof(msg), "tcp_write: %s", strerror(errno));
            mno_fail(msg);
        }
        off += (size_t)n;
    }
    return len;
}

void mno_tcp_close(mno_i64 handle) {
    if (handle <= 0 || (size_t)handle >= g_tcp_cap || !g_tcp[handle].in_use) {
        return;
    }
    close(g_tcp[handle].fd);
    g_tcp[handle].in_use = 0;
    g_tcp[handle].fd = -1;
}
