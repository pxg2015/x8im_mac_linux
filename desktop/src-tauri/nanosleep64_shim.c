/* 垫片：aws-lc-sys 0.46 在 Windows 下调 nanosleep64，但自带的 mingw(MSVCRT) winpthread
   仅导出自带 nanosleep。MSVCRT 目标 time_t 已是 64 位，struct timespec 与 timespec64 布局一致，
   故直接别名即可。仅用于本地发布构建链接期补符号，不影响运行逻辑。 */
struct timespec { long long tv_sec; long long tv_nsec; };
int nanosleep(const struct timespec *req, struct timespec *rem);
int nanosleep64(const struct timespec *req, struct timespec *rem) {
    return nanosleep(req, rem);
}
