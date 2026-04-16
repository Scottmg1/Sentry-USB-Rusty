#!/usr/bin/env python3
import sys
import time
import asyncio
import socket

from nio import AsyncClient, LoginResponse

if len(sys.argv) != 6:
    sys.stderr.write('usage: %s HOMESERVER_URL USERNAME PASSWORD ROOM_ID MESSAGE_TEXT\n' % sys.argv[0])
    sys.exit(1)

(homeserver, username, password, room_id, message) = sys.argv[1:6]

if homeserver.endswith('/'):
    homeserver = homeserver[:-1]

if username.startswith('@'):
    username = username[1:]

if username.find(':') > 0:
    username = username.split(':')[0]

async def main() -> None:
    client = AsyncClient(homeserver, username)
    response = await client.login(password, device_name=socket.gethostname())

    if not isinstance(response, LoginResponse):
        sys.stderr.write('Failed to connect to Matrix server.\n')
        sys.exit(1)

    await client.room_send(
        room_id=room_id,
        message_type="m.room.message",
        content = {
            "msgtype": "m.text",
            "body": message
        }
    )
    await client.sync(timeout=30000)
    sys.exit(0)

asyncio.get_event_loop().run_until_complete(main())
