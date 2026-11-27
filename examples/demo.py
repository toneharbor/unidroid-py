from unidroid._unidroid import Emulator, JniValue
import os
import json

BASE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RESOURCES = os.path.join(BASE, 'sources')
ROOTFS = os.path.join(RESOURCES, 'rootfs')
CERT_PATH = os.path.join(RESOURCES, 'ms_16777218.bin')

LICENSE_KEY = 'q3z+hZi34gBVOw4rv/eL+jFJm+Q+zE3MvJoktte78kS90bdjsJ19O7BIlx7rZaeDs1IyTzv5OWJUsz+ti5BwDxlSY40IvqM5Fg2hiBypFPtx41j/9hRTmMHDmRROO5bzaC3VnQ4gmodPZooZEURlwbD+S6JVHWOgUbqURyaqowOX+rLmF3jTVCYZbmKACygx6jDr04JlDzP759HHq5Jc/qAnu7O0BVw25m2jeXZs6RfJIHQpviq6rEe6n468emw0UujdbPh1vq0k2TnAhgT3px3bZ4cPmtrJkqIzBMp20meMM60H+sQwzNH19SLwYN3wWuCFXmtxS2aosC2pfxjnHHRJNXmLxDxG+2xRoor0aXSTRrQ2'

APP_ID = '1967'
VERSION = '7.1.3.32'
PROCESS_NAME = 'com.dragon.read'
SDK_VERSION = '29'


class _MetaSecHandler:
    def __init__(self, emu):
        self._emu = emu
        self._msdata_str = emu.new_string('/data/user/0/com.dragon.read/files/.msdata')
        self._version_str = emu.new_string(VERSION)
        with open(CERT_PATH, 'rb') as f:
            cert_hex = f.read()
        self._cert_bytes = emu.new_byte_array(cert_hex)

    def on_import(self, emu, call):
        return None

    def stopped(self, emu, stop):
        return 'Continue'

    def is_assignable(self, emu, from_class, to_class):
        return None

    def member(self, emu, call):
        type_code = call.get('type_code', 'V')
        op = call.get('op', '')
        cls = call.get('class', '')
        name = call.get('name', '')
        args = call.get('args', [])

        if op == 'call' and cls == 'com/bytedance/mobsec/metasec/ml/MS' and name == 'b':
            method_id = args[0] if args else 0
            if method_id == 0x10003:
                return self._msdata_str
            elif method_id == 0x1000002:
                return self._cert_bytes
            elif method_id == 0x1000000E:
                return 0
            elif method_id == 0x1000011:
                return self._version_str

        if type_code in ('L', '['):
            return 0
        elif type_code == 'Z':
            return False
        elif type_code in ('B', 'C', 'S', 'I', 'J'):
            return 0
        elif type_code in ('F', 'D'):
            return 0.0
        return None


class Signer:
    def __init__(self):
        self.emu = Emulator(
            rootfs=ROOTFS,
            process_name=PROCESS_NAME,
            sdk_version=SDK_VERSION,
        )
        self.handler = _MetaSecHandler(self.emu)
        self._init()

    def _init(self):
        emu = self.emu
        handler = self.handler

        libc_path = os.path.join(ROOTFS, 'lib64', 'libc.so')
        emu.load_library(libc_path)
        libmetasec_path = os.path.join(ROOTFS, 'lib64', 'libmetasec_ml.so')
        self.module = emu.load_library(libmetasec_path)

        emu.superclass('com/bytedance/mobsec/metasec/ml/MS', 'ms/bd/c/i2')
        emu.superclass('ms/bd/c/i2', 'ms/bd/c/y2')
        emu.superclass('ms/bd/c/y2', 'java/lang/Object')

        emu.call_jni_onload(self.module, handler)

        config = json.dumps([
            APP_ID, '', '', LICENSE_KEY, VERSION, '', '', '', '', '',
            '0', '-1', '99999', '', '0', [], [],
        ])
        config_str = emu.new_string(config)

        emu.call_static_jni_method(
            'ms/bd/c/y2',
            'a(IIJLjava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;',
            [
                JniValue.int(0x4000001),
                JniValue.int(0),
                JniValue.long(0),
                JniValue.object(config_str),
                JniValue.object(0),
            ],
            handler,
        )

        session_result = emu.call_static_jni_method(
            'ms/bd/c/y2',
            'a(IIJLjava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;',
            [
                JniValue.int(0x4000002),
                JniValue.int(0),
                JniValue.long(0),
                JniValue.object(0),
                JniValue.object(0),
            ],
            handler,
        )
        session_handle = session_result.value()
        self.session_long = emu.unboxed(session_handle).value()

    def sign(self, url: str, body: bytes = b'') -> dict[str, str]:
        emu = self.emu
        handler = self.handler

        url_str = emu.new_string(url)
        body_bytes = emu.new_byte_array(body)

        result = emu.call_static_jni_method(
            'ms/bd/c/y2',
            'a(IIJLjava/lang/String;Ljava/lang/Object;)Ljava/lang/Object;',
            [
                JniValue.int(0x2000006),
                JniValue.int(0),
                JniValue.long(self.session_long),
                JniValue.object(url_str),
                JniValue.object(body_bytes),
            ],
            handler,
        )

        result_handle = result.value()
        handles = emu.object_array(result_handle)

        sign_headers = {}
        for i in range(0, len(handles), 2):
            key = emu.string(handles[i])
            val = emu.string(handles[i + 1])
            sign_headers[key] = val

        return sign_headers


if __name__ == '__main__':
    signer = Signer()

    url = 'https://novel.snssdk.com/reading/bookapi/newhome/v/?aid=1967&version_code=71332'
    headers = signer.sign(url)

    print()
    print('========== 番茄小说签名输出 ==========')
    for k, v in headers.items():
        print(f'{k}: {v}')
    print('========================================')