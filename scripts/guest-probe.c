/* Optional smoke-test payload, cross-compiled locally; never part of the VMM. */
#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    int cpus = atoi(argv[1]), gate[2];
    size_t bytes = (size_t)atoi(argv[2]) * 1024 * 1024;
    if (cpus < 1 || cpus > 64 || pipe(gate)) return 2;
    for (int i = 0; i < cpus; ++i) {
        pid_t child = fork();
        if (child < 0) return 3;
        if (child == 0) {
            close(gate[1]);
            cpu_set_t set; CPU_ZERO(&set); CPU_SET(i, &set);
            char start;
            if (sched_setaffinity(0, sizeof(set), &set) || read(gate[0], &start, 1) != 1) _exit(4);
            volatile unsigned long result = 1;
            for (int n = 0; n < 10000000; ++n) result = result * 1664525 + 1013904223;
            if (sched_getcpu() != i) _exit(5);
            printf("PINNED_CPU_%d_OK\n", i); fflush(stdout); _exit(0);
        }
    }
    close(gate[0]);
    for (int i = 0; i < cpus; ++i) if (write(gate[1], "x", 1) != 1) return 6;
    close(gate[1]);
    for (int i = 0, status; i < cpus; ++i) if (wait(&status) < 0 || !WIFEXITED(status) || WEXITSTATUS(status)) return 7;
    if (bytes) {
        unsigned char *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) return 8;
        for (size_t i = 0; i < bytes; ++i) p[i] = (unsigned char)(i ^ (i >> 12));
        for (size_t i = 0; i < bytes; ++i) if (p[i] != (unsigned char)(i ^ (i >> 12))) return 9;
        printf("VERIFIED_%zu_MIB\n", bytes >> 20);
        if (munmap(p, bytes)) return 10;
    }
    return 0;
}
