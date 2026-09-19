"""Representative Python fixture: classes, methods, async, imports, calls."""

import asyncio
from protocol import Packet
from . import peer
from ..lib import make_id


@dataclass
class Client(Base):
    def __init__(self, transport):
        self.transport = transport

    async def connect(self):
        await asyncio.sleep(1)

    def parse(self, data):
        return parse_packet(data)


def make_client(config):
    return Client(config)
