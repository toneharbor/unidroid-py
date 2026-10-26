import unidroid
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


class Handler:
    def on_import(self, emu, call):
        return None

    def stopped(self, emu, stop):
        return "Continue"

    def is_assignable(self, emu, from_class, to_class):
        return None

    def member(self, emu, call):
        type_code = call.get("type_code", "V")
        if type_code in ("L", "["):
            return 0
        elif type_code == "Z":
            return False
        elif type_code == "B":
            return 0
        elif type_code == "C":
            return 0
        elif type_code == "S":
            return 0
        elif type_code == "I":
            return 0
        elif type_code == "J":
            return 0
        elif type_code == "F":
            return 0.0
        elif type_code == "D":
            return 0.0
        return None


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

    print("=== Load rootfs libc.so ===")
    libc_path = os.path.join(ROOTFS, "lib64", "libc.so")
    libc = emu.load_library(libc_path)
    print(f"  libc module handle: {libc}")
    print()

    print("=== Load APK Library: libmetasec_ml.so ===")
    module = emu.load_apk_library("libmetasec_ml.so")
    print(f"  module handle: {module}")
    print()

    print("=== Declare Superclass ===")
    emu.superclass("com/bytedance/mobsec/metasec/ml/MS", "java/lang/Object")
    print("  com/bytedance/mobsec/metasec/ml/MS -> java/lang/Object")
    print()

    print("=== Find JNI_OnLoad ===")
    addr = emu.find_symbol(module, "JNI_OnLoad")
    print(f"  JNI_OnLoad = 0x{addr:x}")
    print()

    print("=== Call JNI_OnLoad ===")
    result = emu.call_jni_onload(module, handler)
    print(f"  JNI_OnLoad returned: {result}")
    print()

    print("=== Registered Natives ===")
    natives = emu.registered_natives()
    print(f"  registered natives: {len(natives)}")
    for cls, method, addr in natives:
        print(f"    {cls}.{method} -> 0x{addr:x}")
    print()

    print("=== Crypto Constants ===")
    try:
        crypto = emu.crypto_constants()
        print(f"  crypto constants: {len(crypto)}")
        for c in crypto[:20]:
            print(f"    {c.name} @ 0x{c.address:x} ({c.location})")
    except RuntimeError as e:
        print(f"  error: {e}")
    print()

    print("=== Modules ===")
    modules = emu.modules()
    for m in modules:
        print(f"  {m.name} base=0x{m.base:x} size=0x{m.size:x}")
    print()

    print("=== Memory Maps (first 20) ===")
    maps = emu.maps()
    for m in maps[:20]:
        print(f"  0x{m.start:x}-0x{m.end:x} {m.perms:4s} {m.name}")
    if len(maps) > 20:
        print(f"  ... ({len(maps) - 20} more)")
    print()

    print("=== Call Registered Native Method ===")
    if natives:
        cls, method, addr = natives[0]
        print(f"  calling {cls}.{method} @ 0x{addr:x}")
        try:
            jni_args = [unidroid.JniValue.object(0)]
            result = emu.call_jni_method(cls, 0, method, jni_args, handler)
            print(f"  result: {result}")
        except RuntimeError as e:
            print(f"  error: {e}")
    print()

    print("Done.")


if __name__ == "__main__":
    main()