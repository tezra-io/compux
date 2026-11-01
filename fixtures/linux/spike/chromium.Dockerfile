# M86 L0(a), Chromium: Debian's own `chromium` package (no snap) in a private session
# like the fixtures' (chromium_session.sh: Xvfb, openbox, its own session and
# accessibility bus), showing one local page of controls (chromium_page.html). The
# compux helper and the probes come from the spike build. `spike.sh` builds this;
# nothing here reaches a person's desktop, and the session runs with no network.
ARG BASE=debian:bookworm
FROM ${BASE}
USER root
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      chromium xvfb openbox dbus at-spi2-core python3 python3-gi gir1.2-glib-2.0 \
      libxdo3 libxkbcommon-x11-0 libxcb-shape0 libxcb-xfixes0 libxcb-render0 \
      fonts-dejavu-core \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --shell /bin/bash tester
COPY compux /opt/compux/compux
COPY elements_timing.py a11y_tree.py chromium_session.sh chromium_page.html /opt/compux-spike/
USER tester
WORKDIR /home/tester
