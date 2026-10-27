import unidroid
from unidroid import DefaultHandler, JniCall
import os
import sys

APK_PATH = os.path.join(os.path.dirname(__file__), "..", "sources", "Read_7.1.3.32_APKPure.apk")
APK_PATH = os.path.abspath(APK_PATH)
ROOTFS = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "unidroid", "rootfs"))

if not os.path.isfile(APK_PATH):
    print(f"APK not found: {APK_PATH}", file=sys.stderr)
    print("Place your APK file in the sources/ directory:", file=sys.stderr)
    print("  sources/Read_7.1.3.32_APKPure.apk", file=sys.stderr)
    sys.exit(1)

if not os.path.isdir(ROOTFS):
    print(f"rootfs not found: {ROOTFS}", file=sys.stderr)
    print("Clone unidroid repo to parent directory, or set ROOTFS env var:", file=sys.stderr)
    print("  git clone https://github.com/toneharbor/unidroid.git ../unidroid", file=sys.stderr)
    sys.exit(1)


class Handler(DefaultHandler):
    """Fine-grained JNI handler — override only what you need."""
    def on_call_static_method(self, emu, call):
        print(f"  [JNI] static: {call.class_name}.{call.name}{call.signature}")
        return call.default_value

    def on_call_instance_method(self, emu, call):
        print(f"  [JNI] instance: {call.class_name}.{call.name}{call.signature}")
        return call.default_value


def main():
    print(f"APK: {APK_PATH}")
    print(f"rootfs: {ROOTFS}")
    print()

    emu = unidroid.Emulator(
        rootfs=ROOTFS,
        apk=APK_PATH,
        process_name="com.ss.android.auto.ugc.aweme",
        sdk_version="29",
    )
    print(f"process_name: {emu.process_name}")
    print()

    handler = Handler()

    print("=== Load APK Library: libEncryptor.so ===")
    try:
        module = emu.load_apk_library("libEncryptor.so")
        print(f"  module handle: {module}")
    except RuntimeError as e:
        print(f"  error: {e}")
        module = None
    print()

    print("=== Load APK Library (no constructors): libsscronet.so ===")
    try:
        module2 = emu.load_apk_library_without_constructors("libsscronet.so")
        print(f"  module handle: {module2}")
    except RuntimeError as e:
        print(f"  error: {e}")
    print()

    print("=== Modules ===")
    modules = emu.modules()
    for m in modules:
        print(f"  {m.name} base=0x{m.base:x} size=0x{m.size:x} path={m.path}")
    print()

    print("=== Memory Maps (first 20) ===")
    maps = emu.maps()
    for m in maps[:20]:
        print(f"  0x{m.start:x}-0x{m.end:x} {m.perms:4s} {m.name}")
    if len(maps) > 20:
        print(f"  ... ({len(maps) - 20} more)")
    print()

    if module is not None:
        print("=== Find Symbol ===")
        try:
            addr = emu.find_symbol(module, "JNI_OnLoad")
            print(f"  JNI_OnLoad = 0x{addr:x}")
        except RuntimeError as e:
            print(f"  error: {e}")
        print()

        print("=== Call JNI_OnLoad ===")
        try:
            result = emu.call_jni_onload(module, handler)
            print(f"  JNI_OnLoad returned: {result}")
        except RuntimeError as e:
            print(f"  error: {e}")
        print()

    print("=== Heap Info ===")
    hwm = emu.heap_high_water_mark()
    print(f"  heap high water mark: 0x{hwm:x}")
    allocations = emu.heap()
    live = [a for a in allocations if a.live]
    print(f"  total allocations: {len(allocations)}, live: {len(live)}")
    print()

    print("=== Coverage ===")
    coverage = emu.coverage()
    print(f"  coverage blocks: {len(coverage)}")
    for b in coverage[:10]:
        print(f"    0x{b.address:x} {b.location} count={b.count}")
    print()

    print("=== Crypto Constants ===")
    try:
        crypto = emu.crypto_constants()
        print(f"  crypto constants: {len(crypto)}")
        for c in crypto[:10]:
            print(f"    {c.name} @ 0x{c.address:x} ({c.location})")
    except RuntimeError as e:
        print(f"  error: {e}")
    print()

    print("=== Trace ===")
    trace = emu.trace()
    print(f"  jni={trace.jni} syscalls={trace.syscalls} imports={trace.imports} heap={trace.heap}")
    print()

    print("=== Registered Natives ===")
    natives = emu.registered_natives()
    print(f"  registered natives: {len(natives)}")
    for cls, method, addr in natives[:10]:
        print(f"    {cls}.{method} -> 0x{addr:x}")
    print()

    print("=== Test call_static_jni_method (ttEncrypt) ===")
    if natives:
        cls, method_sig, addr = natives[0]
        print(f"  calling {cls}.{method_sig}")
        try:
            byte_arr = emu.new_byte_array(b"hello world")
            arr_val = unidroid.JniValue.object(byte_arr)
            i_val = unidroid.JniValue.int(1)
            result = emu.call_static_jni_method(cls, method_sig, [arr_val, i_val], handler)
            print(f"  result: {result}")
            if result.kind() == "Object":
                encrypted = emu.byte_array(result.value())
                print(f"  encrypted len: {len(encrypted)}")
                print(f"  encrypted hex: {encrypted.hex()}")
        except RuntimeError as e:
            print(f"  error: {e}")
    else:
        print("  (no registered natives)")
    print()

    print("Done.")


if __name__ == "__main__":
    main()