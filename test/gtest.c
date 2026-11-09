/* Kontrolny program glibc do testów glibc-run.
 * - gnu_get_libc_version(): symbol czysto glibc-owy (musl go nie ma) — dowodzi,
 *   że naprawdę działamy na glibc, a nie na musl.
 * - otwiera zaszytą ścieżkę /etc/glibc-run-test/marker — test wirtualizacji. */
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
        /* utnij ewentualny newline */
        for (size_t i = 0; i < n; i++) if (buf[i] == '\n') { buf[i] = 0; break; }
        printf("marker=%s\n", buf);
        fclose(f);
    } else {
        printf("marker=MISSING\n");
    }
    return 0;
}
