# sha256 of each per-target release tarball: %{"<target>" => "<hex sha256>"}.
# The release CI (P3) regenerates this file at tag time; it is a placeholder empty
# map until the first release. `Compux.Binary` bakes it in at compile time and
# verifies every download against it (rustler_precompiled-style).
%{}
