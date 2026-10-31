# A private Linux desktop session for the fixture applications.
#
# Everything a Linux accessibility test needs, and nothing of the person's: an X
# server with no screen (Xvfb), a window manager (openbox), a session bus that
# exists only inside the container (dbus-run-session), the accessibility bus
# (at-spi2-core), and the three fixture applications with their toolkits. The
# owner's desktop is never reached: no socket from the host is mounted, and
# `private-session.sh` starts every bus and server it uses itself (M86 R10).
#
#   docker build -t compux-fixtures:noble fixtures/linux
#   docker build -t compux-fixtures:jammy --build-arg BASE=ubuntu:22.04 \
#     --build-arg QT_PACKAGES= --build-arg FIXTURES="gtk3 gtk4" fixtures/linux
#
#   docker run --rm compux-fixtures:noble private-session.sh --self-test
#   docker run --rm compux-fixtures:noble private-session.sh python3 /opt/compux-fixtures/a11y_roots.py \
#     --expect "Compux Fixture GTK3" --expect "Compux Fixture GTK4" --expect "Compux Fixture Qt"
#   (a bare listing can run before GTK4 has registered: the --expect wait is the proof)
#
# The jammy build is M86 L0(b)'s: Ubuntu 22.04 ships at-spi2-core 2.44 and the ATK
# bridge 2.38 that aborts a GTK3 application asked `Action.GetActions`, which is
# the owner's own desktop. It has no packaged PyQt6 or PySide6, so it carries the
# two GTK fixtures only, and says so in COMPUX_FIXTURES.
#
# xdotool and busctl are here for ORACLES and probes inside the container (what is
# the active window, what is on the bus), never as a way to drive the fixtures:
# the thing under test drives them. The `systemd` package is installed only for
# `busctl`; nothing runs systemd in the container.
ARG BASE=ubuntu:24.04
FROM ${BASE}

ARG QT_PACKAGES="python3-pyqt6 qt6-qpa-plugins"
ARG FIXTURES="gtk3 gtk4 qt"

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      xvfb openbox dbus at-spi2-core \
      python3 python3-gi gir1.2-gtk-3.0 gir1.2-gtk-4.0 \
      xdotool systemd fonts-dejavu-core \
      ${QT_PACKAGES} \
 && rm -rf /var/lib/apt/lists/*

COPY fixture_gtk3.py fixture_gtk4.py fixture_qt.py a11y_roots.py /opt/compux-fixtures/
COPY private-session.sh /usr/local/bin/private-session.sh

ENV COMPUX_FIXTURES="${FIXTURES}" \
    COMPUX_FIXTURE_DIR=/opt/compux-fixtures

# Nothing here runs as root that does not need to: the session, its buses and the
# applications all belong to an ordinary account, as they would on a desktop.
RUN useradd --create-home --shell /bin/bash tester
USER tester
WORKDIR /home/tester
