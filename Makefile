# glibc-run — GNU make (gmake). Usage:
#   gmake                 # build (cargo release)
#   sudo gmake install    # install into /usr/local
#   sudo gmake uninstall
# Variables: PREFIX=/usr/local, DESTDIR= (for packaging), TARGET= (cargo triple)

# When make runs under an application started via glibc-run (e.g. a terminal
# in Claude Desktop), it inherits LD_LIBRARY_PATH with the glibc libraries —
# which breaks musl tools (cargo, install). Do not pass it on.
LD_LIBRARY_PATH =
unexport LD_LIBRARY_PATH

PREFIX  ?= /usr/local
BINDIR   = $(PREFIX)/bin
SHAREDIR = $(PREFIX)/share/glibc-run
DOCDIR   = $(PREFIX)/share/doc/glibc-run

# On musl (Chimera) build natively; on glibc target musl,
# so a single binary works both on the host and in musl containers.
ifeq ($(wildcard /lib/ld-musl-x86_64.so.1),)
  TARGET ?= x86_64-unknown-linux-musl
  BIN     = target/$(TARGET)/release/glibc-run
  CARGOFLAGS = --release --target $(TARGET)
else
  TARGET ?=
  BIN     = target/release/glibc-run
  CARGOFLAGS = --release
endif

.PHONY: all build install uninstall clean

all: build

build:
	cargo build $(CARGOFLAGS)

# Deliberately no dependency on build: `sudo gmake install` must never
# build as root (permissions in target/ and ~/.cargo).
install:
	@test -x "$(BIN)" || { echo "Missing $(BIN) — run gmake first"; exit 1; }
	mkdir -p "$(DESTDIR)$(BINDIR)" "$(DESTDIR)$(SHAREDIR)" "$(DESTDIR)$(DOCDIR)"
	install -m 755 "$(BIN)" "$(DESTDIR)$(BINDIR)/glibc-run"
	install -m 755 setup-runtime.sh "$(DESTDIR)$(SHAREDIR)/setup-runtime.sh"
	install -m 644 README.md "$(DESTDIR)$(DOCDIR)/README.md"
	install -m 644 LICENSE "$(DESTDIR)$(DOCDIR)/LICENSE"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/glibc-run"
	rm -rf "$(DESTDIR)$(SHAREDIR)" "$(DESTDIR)$(DOCDIR)"

clean:
	cargo clean
