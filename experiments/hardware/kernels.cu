// SPDX-License-Identifier: AGPL-3.0-or-later
// A port of rules::space::first_hit and rules::rng::Rng, not a board engine.
// Compile with --fmad=false and retain FP64. The CPU reference is authoritative.
#include "portable_hypot.cuh"

extern "C" __global__ void rng_draws(unsigned long long seed, unsigned long long offset,
                                    unsigned int count, double *out) {
    unsigned int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    unsigned long long z = seed + (offset + i + 1ULL) * 0x9e3779b97f4a7c15ULL;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    z ^= z >> 31;
    out[i] = (double)(z >> 11) / 9007199254740992.0;
}

extern "C" __global__ void first_hits(const double *bodies, unsigned int body_count,
                                     const double *rays, unsigned int count,
                                     unsigned long long base_gas, unsigned long long loop_gas,
                                     unsigned long long compare_gas,
                                     unsigned long long *out) {
    unsigned int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    const double *ray = rays + 4ULL * i;
    double len = portable_hypot(ray[2], ray[3]);
    unsigned long long best = ~0ULL, gas = base_gas;
    double distance = 0.0;
    if (!(len <= 0.0)) {
        double ux = ray[2] / len, uy = ray[3] / len;
        for (unsigned int b = 0; b < body_count; ++b) {
            gas += loop_gas;
            double px = bodies[2ULL * b] - ray[0], py = bodies[2ULL * b + 1] - ray[1];
            double along = px * ux + py * uy;
            if (along < 0.0) continue;
            double perp = fabs(px * uy - py * ux);
            if (perp > 0.25) continue;
            double half = sqrt(fmax(0.25 * 0.25 - perp * perp, 0.0));
            double entry = fmax(along - half, 0.0);
            if (best != ~0ULL) gas += compare_gas;
            if (best == ~0ULL || entry < distance) {
                best = b;
                distance = entry;
            }
        }
    }
    out[3ULL * i] = best;
    out[3ULL * i + 1] = __double_as_longlong(distance);
    out[3ULL * i + 2] = gas;
}
