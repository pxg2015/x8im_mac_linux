/* 垫片：aws-lc-sys 0.46 在 Windows 下调 nanosleep64，但自带的 mingw(MSVCRT) winpthread
   仅导出自带 nanosleep。MSVCRT 目标 time_t 已是 64 位，struct timespec 与 timespec64 布局一致，
   故直接别名即可。仅用于本地发布构建链接期补符号，不影响运行逻辑。 */
struct timespec { long long tv_sec; long long tv_nsec; };
int nanosleep(const struct timespec *req, struct timespec *rem);
/* 2026-10-28：原为 __attribute__((weak))。GCC 16.2 + `-Wl,--gc-sections` 下，
   只有 weak 定义的段会被判为不可达而回收 → 链接期报
   `undefined reference to nanosleep64`（1.6.43 构建实测）。
   改成强定义：本 .o 直接提供符号，ld 不再从 libpthread.a 抽取同名对象，
   因此不会与 GCC16 自带的强符号冲突（那份 nanosleep.o 已无需被抽取）。 */
int nanosleep64(const struct timespec *req, struct timespec *rem) {
    return nanosleep(req, rem);
}
