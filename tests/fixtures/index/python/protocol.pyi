"""Stub-only Python module (.pyi)."""

class Packet:
    raw: bytes

def parse_packet(raw: bytes) -> Packet: ...
def make_id(prefix: str) -> str: ...
