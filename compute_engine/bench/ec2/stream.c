#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>

static double *a, *b;
static size_t n, threads;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void *work(void *arg) {
    size_t id = (size_t)arg, share = (n + threads - 1) / threads;
    size_t start = id * share, end = start + share < n ? start + share : n;
    for (size_t i = start; i < end; i++) b[i] = a[i] * 3.0;
    return NULL;
}

static double run(void) {
    pthread_t pool[64];
    double started = now();
    for (size_t t = 0; t < threads; t++) pthread_create(&pool[t], NULL, work, (void *)t);
    for (size_t t = 0; t < threads; t++) pthread_join(pool[t], NULL);
    return now() - started;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: stream <threads> <megabytes per array>\n");
        return 2;
    }
    threads = atoi(argv[1]);
    n = (size_t)atoi(argv[2]) * 1024 * 1024 / sizeof(double);
    a = malloc(n * sizeof(double));
    for (size_t i = 0; i < n; i++) a[i] = i;
    b = malloc(n * sizeof(double));
    for (size_t i = 0; i < n; i++) b[i] = 0;
    double warm = 1e9, fresh = 1e9;
    for (int round = 0; round < 5; round++) {
        double t = run();
        if (t < warm) warm = t;
        double *keep = b;
        b = mmap(NULL, n * sizeof(double), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        t = run();
        if (t < fresh) fresh = t;
        munmap(b, n * sizeof(double));
        b = keep;
    }
    double bytes = 2.0 * n * sizeof(double);
    printf("threads %zu\twarm %.1f GB/s\tfresh %.1f GB/s\n", threads, bytes / warm / 1e9, bytes / fresh / 1e9);
    return 0;
}
