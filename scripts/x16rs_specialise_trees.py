#!/usr/bin/env python3
"""Build compile-time-specialised OpenCL kernel trees for the x16rs gate.

Unlike `x16rs_gate_trees.py`, every tree here is meant to hash CORRECTLY. The
only difference from the shipping tree is that `X16RS_UNIT_SIZE` and
`X16RS_LOCAL_SIZE` are defined at the top of `x16rs_main.cl`, which turns the
kernel's unit size and work-group size from a runtime argument and a runtime
query into literals the compiler can see.

A tree built here is valid at EXACTLY ONE launch shape. Launching it at any
other unit size gives wrong hashes, so each one has to pass `x16rs_gate equiv`
at its own shape before any number from it is reported.

    python scripts/x16rs_specialise_trees.py x16rs/opencl <outdir> 64 128 192

writes <outdir>/us64, <outdir>/us128, <outdir>/us192 (local_size 256), plus
<outdir>/stock, an unspecialised copy, so an A/B run compares two trees that
were both freshly compiled from source rather than one of them from the
shipping tree's binary cache.
"""
import os
import shutil
import sys

MARKER = "#include \"sha3_256.cl\"\n"


def copy_tree(src, dst):
    if os.path.isdir(dst):
        shutil.rmtree(dst)
    os.makedirs(dst)
    for name in os.listdir(src):
        if name.endswith(".cl"):
            shutil.copy(os.path.join(src, name), os.path.join(dst, name))


def specialise(src, dst, unit_size, local_size):
    copy_tree(src, dst)
    path = os.path.join(dst, "x16rs_main.cl")
    text = open(path, encoding="utf-8").read()
    assert text.count(MARKER) == 1, "x16rs_main.cl include block moved"
    if unit_size is None and local_size is None:
        return
    defines = ""
    if unit_size is not None:
        defines += "#define X16RS_UNIT_SIZE %d\n" % unit_size
    if local_size is not None:
        defines += "#define X16RS_LOCAL_SIZE %d\n" % local_size
    text = text.replace(MARKER, MARKER + "\n" + defines, 1)
    # The host also compiles with -D; putting the defines in the SOURCE is what
    # makes compile.rs's content fingerprint differ, so the tree cannot pick up
    # another tree's cached binary.
    open(path, "w", encoding="utf-8", newline="\n").write(text)


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    src, out = sys.argv[1], sys.argv[2]
    units = [int(a) for a in sys.argv[3:]]
    os.makedirs(out, exist_ok=True)
    specialise(src, os.path.join(out, "stock"), None, None)
    print("stock (no defines) ->", os.path.join(out, "stock"))
    for unit_size in units:
        dst = os.path.join(out, "us%d" % unit_size)
        specialise(src, dst, unit_size, 256)
        print("unit_size=%d local_size=256 -> %s" % (unit_size, dst))
    return 0


if __name__ == "__main__":
    sys.exit(main())
