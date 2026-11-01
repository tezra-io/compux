# The fixture image (fixtures/linux/Dockerfile) plus the compux helper and the
# spike's probes, for M86 L0(a) and L0(b), L1's verbs, L2's pixels and L3's bound
# window, with the badge beside the helper as the release archive has it. Everything
# still runs inside private-session.sh: a private Xvfb, a private session bus, its
# own accessibility bus. `spike.sh` builds this; nothing here reaches a person's
# desktop.
ARG BASE=compux-fixtures:noble
FROM ${BASE}
# The helper links nothing past the C runtime (M86 L2), so no library is added.
USER root
COPY compux compux-indicator-x11 /opt/compux/
COPY elements_timing.py a11y_tree.py measure.sh atk_bridge_check.sh verbs.py pixels.py \
     x11_oracle.py bound.py panes.py xcb_victim.py /opt/compux-spike/
USER tester
