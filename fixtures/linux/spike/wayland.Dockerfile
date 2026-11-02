# M86 L0(a), the Wayland half: the spike image (spike.Dockerfile) plus GNOME Shell,
# run headless on Wayland with no X11 (wayland_session.sh), its own session and
# accessibility bus. `spike.sh` builds this; nothing here reaches a person's desktop.
ARG BASE=compux-spike:noble
FROM ${BASE}
USER root
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      gnome-shell libgl1-mesa-dri \
 && rm -rf /var/lib/apt/lists/*
COPY wayland_session.sh /opt/compux-spike/
USER tester
