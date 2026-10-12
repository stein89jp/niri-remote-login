CC ?= cc
CFLAGS ?= -O2 -g
CPPFLAGS += -D_FORTIFY_SOURCE=3
WARNINGS = -std=c17 -Wall -Wextra -Werror -Wformat=2 -Wshadow -Wconversion
LDLIBS = -lpam -lsystemd

all: niri-rdp-broker
niri-rdp-broker: broker.c
	$(CC) $(CPPFLAGS) $(CFLAGS) $(WARNINGS) $< $(LDFLAGS) $(LDLIBS) -o $@
broker-tests: tests.c broker.c
	$(CC) $(CPPFLAGS) $(CFLAGS) $(WARNINGS) -Wno-unused-function tests.c $(LDFLAGS) $(LDLIBS) -o $@
check: broker-tests
	./broker-tests
clean:
	rm -f niri-rdp-broker broker-tests
install: niri-rdp-broker
	@case "$(DESTDIR)" in /*) ;; *) printf '%s\n' 'Set an absolute staging DESTDIR; direct host installation is disabled.' >&2; exit 1 ;; esac
	install -Dm755 niri-rdp-broker "$(DESTDIR)/usr/lib/niri-rdp/niri-rdp-broker"
	install -Dm755 config/session-worker "$(DESTDIR)/usr/lib/niri-rdp/session-worker"
	install -Dm755 config/session-audio "$(DESTDIR)/usr/lib/niri-rdp/session-audio"
	install -Dm755 config/session-desktop "$(DESTDIR)/usr/lib/niri-rdp/session-desktop"
	install -Dm755 config/session-portals "$(DESTDIR)/usr/lib/niri-rdp/session-portals"
	install -Dm755 config/session-environment "$(DESTDIR)/usr/lib/niri-rdp/session-environment"
	install -Dm755 config/session-autostart "$(DESTDIR)/usr/lib/niri-rdp/session-autostart"
	install -Dm644 config/niri-rdp-portals.conf "$(DESTDIR)/usr/share/xdg-desktop-portal/niri-rdp-portals.conf"
	install -Dm644 config/niri-rdp.pam "$(DESTDIR)/etc/pam.d/niri-rdp"
	install -Dm644 config/headless.kdl "$(DESTDIR)/etc/niri-rdp/headless.kdl"
	install -Dm644 config/waybar.json "$(DESTDIR)/etc/niri-rdp/waybar.json"
	install -Dm644 config/renderer.conf "$(DESTDIR)/etc/niri-rdp/renderer.conf"
	install -Dm644 config/niri-rdp-broker.service "$(DESTDIR)/usr/lib/systemd/system/niri-rdp-broker.service"
.PHONY: all check clean install
