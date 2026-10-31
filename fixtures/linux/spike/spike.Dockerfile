# The fixture image (fixtures/linux/Dockerfile) plus the compux helper and the
# spike's probes, for M86 L0(a) and L0(b). Everything still runs inside
# private-session.sh: a private Xvfb, a private session bus, its own accessibility
# bus. `spike.sh` builds this; nothing here reaches a person's desktop.
ARG BASE=compux-fixtures:noble
FROM ${BASE}
USER root
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      libxdo3 libxkbcommon-x11-0 libxcb-shape0 libxcb-xfixes0 libxcb-render0 \
 && rm -rf /var/lib/apt/lists/*
COPY compux /opt/compux/compux
COPY elements_timing.py a11y_tree.py measure.sh atk_bridge_check.sh /opt/compux-spike/
USER tester
