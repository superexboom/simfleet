# SPDX-License-Identifier: AGPL-3.0-or-later
import ctypes as c
import json


class Wire:
    def __init__(self, path):
        self.lib = c.CDLL(str(path.resolve()))
        self.lib.sf_wire_decode.argtypes = [c.c_void_p, c.c_size_t, c.POINTER(c.c_size_t)]
        self.lib.sf_wire_decode.restype = c.c_void_p
        self.lib.sf_wire_score.argtypes = [c.c_char_p, c.c_size_t, c.c_void_p, c.c_size_t, c.POINTER(c.c_uint), c.c_uint, c.c_uint, c.POINTER(c.c_size_t)]
        self.lib.sf_wire_score.restype = c.c_void_p
        self.lib.sf_wire_free.argtypes = [c.c_void_p, c.c_size_t]
        self.lib.sf_wire_free.restype = None

    def take(self, ptr, size):
        if not ptr: raise RuntimeError("wire allocation failed")
        try: value = json.loads(c.string_at(ptr, size.value))
        finally: self.lib.sf_wire_free(ptr, size.value)
        if value.get("ok") is False: raise RuntimeError(value)
        return value

    def decode(self, data):
        buf = c.create_string_buffer(data); size = c.c_size_t()
        return self.take(self.lib.sf_wire_decode(buf, len(data), c.byref(size)), size)

    def score(self, request, ruler, data, sizes, stride):
        body = json.dumps(dict(request=request, ruler=ruler)).encode(); length = c.c_size_t()
        return self.take(self.lib.sf_wire_score(body, len(body), data, c.sizeof(data), sizes, len(sizes), stride, c.byref(length)), length)
