/* glibc control program for glibc-run tests.
 * - gnu_get_libc_version(): a purely glibc symbol (musl does not have it) —
 *   proves we are really running on glibc, not on musl.
 * - opens the hardcoded path /etc/glibc-run-test/marker — virtualization test. */
#include <stdio.h>
#include <gnu/libc-version.h>

int main(void) {
    printf("libc=%s\n", gnu_get_libc_version());

    const char *p = "/etc/glibc-run-test/marker";
    FILE *f = fopen(p, "r");
    if (f) {
        char buf[256];
        size_t n = fread(buf, 1, sizeof(buf) - 1, f);
        buf[n] = 0;
        /* strip a trailing newline, if any */
        for (size_t i = 0; i < n; i++) if (buf[i] == '\n') { buf[i] = 0; break; }
        printf("marker=%s\n", buf);
        fclose(f);
    } else {
        printf("marker=MISSING\n");
    }
    return 0;
}
