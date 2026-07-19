#ifndef MACHINO_RT_H
#define MACHINO_RT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int64_t mno_i64;
typedef double mno_f64;

/* Last message passed to mno_fail (valid until process exit). */
extern const char *mno_fail_msg;

void mno_init(int argc, char **argv);
void mno_fail(const char *msg) __attribute__((noreturn));

/* Checked integer arithmetic (trap on overflow / div-by-zero). */
mno_i64 mno_iadd(mno_i64 a, mno_i64 b);
mno_i64 mno_isub(mno_i64 a, mno_i64 b);
mno_i64 mno_imul(mno_i64 a, mno_i64 b);
mno_i64 mno_idiv(mno_i64 a, mno_i64 b);
mno_i64 mno_irem(mno_i64 a, mno_i64 b);

/* Print value + newline. */
void mno_print_i64(mno_i64 v);
void mno_print_f64(mno_f64 v);
void mno_print_bool(mno_i64 v);
void mno_print_str(mno_i64 s);

/* Byte strings (heap pointers stored as mno_i64). */
mno_i64 mno_str_from_lit(const char *bytes, mno_i64 len);
mno_i64 mno_str_len(mno_i64 s);
mno_i64 mno_str_at(mno_i64 s, mno_i64 i);
mno_i64 mno_str_concat(mno_i64 a, mno_i64 b);
mno_i64 mno_str_eq(mno_i64 a, mno_i64 b);
mno_i64 mno_substr(mno_i64 s, mno_i64 start, mno_i64 end);
mno_i64 mno_chr(mno_i64 c);

/* UTF-8 codepoint helpers (indices are Unicode scalar positions). */
mno_i64 mno_len_cp(mno_i64 s);
mno_i64 mno_char_at_cp(mno_i64 s, mno_i64 i);
mno_i64 mno_substr_cp(mno_i64 s, mno_i64 start, mno_i64 end);
mno_i64 mno_chr_cp(mno_i64 cp);

/* Arrays of generic mno_i64 slots (copy-on-push). */
mno_i64 mno_arr_new(mno_i64 len);
mno_i64 mno_arr_len(mno_i64 a);
mno_i64 mno_arr_get(mno_i64 a, mno_i64 i);
void mno_arr_set(mno_i64 a, mno_i64 i, mno_i64 v);
mno_i64 mno_arr_push(mno_i64 a, mno_i64 v);

/* Structs and enums. */
mno_i64 mno_struct_new(mno_i64 nfields);
mno_i64 mno_struct_get(mno_i64 s, mno_i64 idx);
void mno_struct_set(mno_i64 s, mno_i64 idx, mno_i64 v);

mno_i64 mno_enum_new(mno_i64 tag, mno_i64 npayloads);
mno_i64 mno_enum_tag(mno_i64 e);
mno_i64 mno_enum_payload(mno_i64 e, mno_i64 i);
void mno_enum_set_payload(mno_i64 e, mno_i64 i, mno_i64 v);

/* Float bitcast and arithmetic. */
mno_i64 mno_f64_to_bits(mno_f64 v);
mno_f64 mno_bits_to_f64(mno_i64 bits);
mno_f64 mno_fadd(mno_f64 a, mno_f64 b);
mno_f64 mno_fsub(mno_f64 a, mno_f64 b);
mno_f64 mno_fmul(mno_f64 a, mno_f64 b);
mno_f64 mno_fdiv(mno_f64 a, mno_f64 b);

mno_i64 mno_to_int(mno_f64 v);
mno_f64 mno_to_float(mno_i64 v);
mno_i64 mno_hash_str(mno_i64 s);

/* Host externs (machino native runtime surface). */
mno_i64 mno_args(void);
mno_i64 mno_getenv(mno_i64 name);
mno_i64 mno_clock_ms(void);
void mno_sleep_ms(mno_i64 ms);
mno_i64 mno_read_file(mno_i64 path);
mno_i64 mno_write_file(mno_i64 path, mno_i64 data);
mno_i64 mno_file_exists(mno_i64 path);
mno_i64 mno_read_line(void);
void mno_exit(mno_i64 code);
mno_i64 mno_http_get(mno_i64 url);

mno_i64 mno_tcp_listen(mno_i64 port);
mno_i64 mno_tcp_accept(mno_i64 listener);
mno_i64 mno_tcp_read(mno_i64 conn);
mno_i64 mno_tcp_write(mno_i64 conn, mno_i64 data);
void mno_tcp_close(mno_i64 handle);

/* Closures: heap object, slot0 = fn ptr as i64, slots 1.. = captures. */
mno_i64 mno_closure_new(void *fn, mno_i64 ncaptures);
void mno_closure_set(mno_i64 c, mno_i64 i, mno_i64 v);
mno_i64 mno_closure_get(mno_i64 c, mno_i64 i);
void *mno_closure_fn(mno_i64 c);

/* Deep copy for concurrency (kind: 'i'/'b' identity, 'f' bits, 's' string, 'p' pointer). */
mno_i64 mno_value_clone(mno_i64 v, char kind);

/* Tasks (pthreads). ret_kind / arg_kinds: 'i','b','f','s','p'.
 * arg_kinds may be NULL when argc==0; otherwise it is a string of length argc. */
typedef mno_i64 (*mno_task_fn)(mno_i64 *argv);
mno_i64 mno_task_spawn(
    mno_task_fn fn,
    mno_i64 *argv,
    mno_i64 argc,
    char ret_kind,
    const char *arg_kinds
);
mno_i64 mno_task_join_i64(mno_i64 h);
mno_f64 mno_task_join_f64(mno_i64 h);
mno_i64 mno_task_join_str(mno_i64 h);

/* Channels (mutex + condvar, unbounded queue). */
mno_i64 mno_chan_new(void);
void mno_chan_close(mno_i64 id);
void mno_chan_send_i64(mno_i64 id, mno_i64 v);
void mno_chan_send_f64(mno_i64 id, mno_f64 v);
void mno_chan_send_str(mno_i64 id, mno_i64 s);
mno_i64 mno_chan_recv_i64(mno_i64 id);
mno_f64 mno_chan_recv_f64(mno_i64 id);
mno_i64 mno_chan_recv_str(mno_i64 id);

/* Mark-sweep GC (non-moving). Roots are addresses of mno_i64 locals holding
 * heap pointers; push a frame at function entry, add_root for each pointer
 * local/temp, pop_frame before return. Collection runs at safepoints via
 * mno_gc_maybe() or explicitly via mno_gc_collect(). */
void mno_gc_push_frame(void);
void mno_gc_add_root(mno_i64 *slot);
void mno_gc_pop_frame(void);
void mno_gc_collect(void);
void mno_gc_maybe(void);
mno_i64 mno_heap_live_count(void);

#ifdef __cplusplus
}
#endif

#endif /* MACHINO_RT_H */
