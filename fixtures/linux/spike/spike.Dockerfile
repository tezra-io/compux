# The fixture image (fixtures/linux/Dockerfile) plus the compux helper and the
# spike's probes, for M86 L0(a) and L0(b), L1's verbs and L2's pixels. Everything
# still runs inside private-session.sh: a private Xvfb, a private session bus, its
# own accessibility bus. `spike.sh` builds this; nothing here reaches a person's
# desktop.
ARG BASE=compux-fixtures:noble
FROM ${BASE}
# The helper links nothing past the C runtime (M86 L2), so no library is added.
USER root
COPY compux /opt/compux/compux
COPY elements_timing.py a11y_tree.py measure.sh atk_bridge_check.sh verbs.py pixels.py \
     x11_oracle.py /opt/compux-spike/
USER tester
