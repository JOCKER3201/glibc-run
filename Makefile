# glibc-run — GNU make (gmake). Użycie:
#   gmake                 # budowa (cargo release)
#   sudo gmake install    # instalacja do /usr/local
#   sudo gmake uninstall
# Zmienne: PREFIX=/usr/local, DESTDIR= (dla pakietowania), TARGET= (triple cargo)

# Gdy make działa pod aplikacją odpaloną przez glibc-run (np. terminal
# w Claude Desktop), dziedziczy LD_LIBRARY_PATH z bibliotekami glibc —
# co wywraca musl-owe narzędzia (cargo, install). Nie przekazujemy go dalej.
LD_LIBRARY_PATH =
unexport LD_LIBRARY_PATH

PREFIX  ?= /usr/local
BINDIR   = $(PREFIX)/bin
SHAREDIR = $(PREFIX)/share/glibc-run
DOCDIR   = $(PREFIX)/share/doc/glibc-run

# Na musl (Chimera) budujemy natywnie; na glibc celujemy w musl,
# żeby jedna binarka działała i na hoście, i w kontenerach musl.
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

# Celowo bez zależności od build: `sudo gmake install` nie ma prawa
# budować jako root (uprawnienia w target/ i ~/.cargo).
install:
	@test -x "$(BIN)" || { echo "Brak $(BIN) — najpierw uruchom: gmake"; exit 1; }
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
