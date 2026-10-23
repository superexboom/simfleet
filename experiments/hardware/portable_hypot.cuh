// SPDX-License-Identifier: MIT
// Translated from libm 0.2.16, src/math/hypot.rs. See LIBM-LICENSE.txt.
// Preserve the source's operation ordering; CUDA's native hypot may differ.
__device__ void portable_sq(double x, double &hi, double &lo) {
    double xc = x * 134217729.0;
    double xh = x - xc + xc;
    double xl = x - xh;
    hi = x * x;
    lo = xh * xh - hi + 2.0 * xh * xl + xl * xl;
}

__device__ double portable_hypot(double x, double y) {
    unsigned long long xi = __double_as_longlong(x) & 0x7fffffffffffffffULL;
    unsigned long long yi = __double_as_longlong(y) & 0x7fffffffffffffffULL;
    if (xi < yi) { unsigned long long t = xi; xi = yi; yi = t; }
    long long ex = xi >> 52, ey = yi >> 52;
    x = __longlong_as_double(xi); y = __longlong_as_double(yi);
    if (ey == 0x7ff) return y;
    if (ex == 0x7ff || yi == 0) return x;
    if (ex - ey > 64) return x + y;
    double z = 1.0;
    const double large = __longlong_as_double(0x6bb0000000000000ULL);
    const double small = __longlong_as_double(0x1430000000000000ULL);
    if (ex > 0x3ff + 510) { z = large; x *= small; y *= small; }
    else if (ey < 0x3ff - 450) { z = small; x *= large; y *= large; }
    double hx, lx, hy, ly;
    portable_sq(x, hx, lx); portable_sq(y, hy, ly);
    return z * sqrt(ly + lx + hy + hx);
}
