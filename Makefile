# zen — build and install.
#
#   make                              build the binary
#   sudo make install                 install for everyone, under /usr/local
#   make install PREFIX=~/.local      install for yourself, no sudo needed
#   sudo make uninstall               remove it again
#
# DESTDIR is honoured for packaging:  make install DESTDIR=pkg PREFIX=/usr

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
MANDIR ?= $(PREFIX)/share/man
DOCDIR ?= $(PREFIX)/share/doc/zen
LICDIR ?= $(PREFIX)/share/licenses/zen
CARGO  ?= cargo

BIN := target/release/zen

all: build

build:
	$(CARGO) build --release --locked

# Installing never builds, so `sudo make install` cannot leave root-owned
# files in the build directory. Build first, as yourself.
install:
	@test -x $(BIN) || { echo "zen is not built yet: run 'make' first (without sudo)"; exit 1; }
	install -Dm755 $(BIN) $(DESTDIR)$(BINDIR)/zen
	install -Dm644 zen.1 $(DESTDIR)$(MANDIR)/man1/zen.1
	install -Dm644 config.toml $(DESTDIR)$(DOCDIR)/config.toml
	install -Dm644 README.md $(DESTDIR)$(DOCDIR)/README.md
	install -Dm644 LICENSE $(DESTDIR)$(LICDIR)/LICENSE
	@echo "installed $(DESTDIR)$(BINDIR)/zen"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/zen
	rm -f $(DESTDIR)$(MANDIR)/man1/zen.1
	rm -f $(DESTDIR)$(DOCDIR)/config.toml $(DESTDIR)$(DOCDIR)/README.md
	rm -f $(DESTDIR)$(LICDIR)/LICENSE
	-rmdir $(DESTDIR)$(DOCDIR) $(DESTDIR)$(LICDIR) 2>/dev/null

clean:
	$(CARGO) clean

.PHONY: all build install uninstall clean
