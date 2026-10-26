import unidroid
import os

APK_PATH = os.path.join(os.path.dirname(__file__), "..", "sources", "Read_7.1.3.32_APKPure.apk")
APK_PATH = os.path.abspath(APK_PATH)
ROOTFS = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "rootfs"))


class Handler:
    def on_import(self, emu, call):
        return None

    def stopped(self, emu, stop):
        return "Continue"

    def is_assignable(self, emu, from_class, to_class):
        return None

    def new_object(self, emu, call):
        return 0

    def call_object_method(self, emu, call):
        return 0

    def call_static_object_method(self, emu, call):
        return 0

    def get_object_class(self, emu, call):
        return None

    def is_instance_of(self, emu, call):
        return None

    def get_array_length(self, emu, call):
        return 0

    def get_object_array_element(self, emu, call):
        return 0

    def get_static_object_field(self, emu, call):
        return 0

    def get_object_field(self, emu, call):
        return 0

    def set_static_object_field(self, emu, call):
        return None

    def set_object_field(self, emu, call):
        return None

    def to_string(self, emu, call):
        return ""

    def register_natives(self, emu, call):
        return None

    def call_boolean_method(self, emu, call):
        return False

    def call_static_boolean_method(self, emu, call):
        return False

    def get_boolean_field(self, emu, call):
        return False

    def get_static_boolean_field(self, emu, call):
        return False

    def set_boolean_field(self, emu, call):
        return None

    def set_static_boolean_field(self, emu, call):
        return None

    def call_byte_method(self, emu, call):
        return 0

    def call_static_byte_method(self, emu, call):
        return 0

    def get_byte_field(self, emu, call):
        return 0

    def get_static_byte_field(self, emu, call):
        return 0

    def set_byte_field(self, emu, call):
        return None

    def set_static_byte_field(self, emu, call):
        return None

    def call_char_method(self, emu, call):
        return 0

    def call_static_char_method(self, emu, call):
        return 0

    def get_char_field(self, emu, call):
        return 0

    def get_static_char_field(self, emu, call):
        return 0

    def set_char_field(self, emu, call):
        return None

    def set_static_char_field(self, emu, call):
        return None

    def call_short_method(self, emu, call):
        return 0

    def call_static_short_method(self, emu, call):
        return 0

    def get_short_field(self, emu, call):
        return 0

    def get_static_short_field(self, emu, call):
        return 0

    def set_short_field(self, emu, call):
        return None

    def set_static_short_field(self, emu, call):
        return None

    def call_int_method(self, emu, call):
        return 0

    def call_static_int_method(self, emu, call):
        return 0

    def get_int_field(self, emu, call):
        return 0

    def get_static_int_field(self, emu, call):
        return 0

    def set_int_field(self, emu, call):
        return None

    def set_static_int_field(self, emu, call):
        return None

    def call_long_method(self, emu, call):
        return 0

    def call_static_long_method(self, emu, call):
        return 0

    def get_long_field(self, emu, call):
        return 0

    def get_static_long_field(self, emu, call):
        return 0

    def set_long_field(self, emu, call):
        return None

    def set_static_long_field(self, emu, call):
        return None

    def call_float_method(self, emu, call):
        return 0.0

    def call_static_float_method(self, emu, call):
        return 0.0

    def get_float_field(self, emu, call):
        return 0.0

    def get_static_float_field(self, emu, call):
        return 0.0

    def set_float_field(self, emu, call):
        return None

    def set_static_float_field(self, emu, call):
        return None

    def call_double_method(self, emu, call):
        return 0.0

    def call_static_double_method(self, emu, call):
        return 0.0

    def get_double_field(self, emu, call):
        return 0.0

    def get_static_double_field(self, emu, call):
        return 0.0

    def set_double_field(self, emu, call):
        return None

    def set_static_double_field(self, emu, call):
        return None

    def call_void_method(self, emu, call):
        return None

    def call_static_void_method(self, emu, call):
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
            result = emu.call_jni_method(module, handler, addr, jni_args)
            print(f"  result: {result}")
        except RuntimeError as e:
            print(f"  error: {e}")
    print()

    print("Done.")


if __name__ == "__main__":
    main()