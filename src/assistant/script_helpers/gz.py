"""File tools for gz run_script, served by gz's sandbox.

Every call goes to gz over a local socket, so it gets the same folder
sandbox, secret blocking, .gitignore rules, permission prompts, and audit
log as the assistant's single tools. Paths are relative to the workspace.
"""

import json
import os
import socket

__all__ = [
    "ToolError",
    "read_file",
    "list_directory",
    "find_files",
    "search_files",
    "edit_file",
    "write_file",
]


class ToolError(Exception):
    """A gz helper call failed or was denied by the user."""


def _call(tool, **fields):
    host, port = os.environ["GZ_BRIDGE"].rsplit(":", 1)
    request = {
        "token": os.environ["GZ_BRIDGE_TOKEN"],
        "tool": tool,
        "input": {k: v for k, v in fields.items() if v is not None},
    }
    with socket.create_connection((host, int(port))) as conn:
        conn.sendall((json.dumps(request) + "\n").encode("utf-8"))
        with conn.makefile("rb") as reader:
            line = reader.readline()
    if not line:
        raise ToolError("gz closed the connection without answering")
    reply = json.loads(line)
    if not reply.get("ok"):
        raise ToolError(reply.get("error", "the call failed"))
    return reply["result"]


def read_file(path, offset=None, limit=None):
    """File text (no line numbers); offset/limit select lines (1-based)."""
    return _call("read_file", path=path, offset=offset, limit=limit)


def list_directory(path="."):
    """Entry names; folders end with '/'."""
    return _call("list_directory", path=path)


def find_files(pattern, path=None):
    """Paths matching a glob such as '**/*.sql'."""
    return _call("find_files", pattern=pattern, path=path)


def search_files(pattern, path=None, glob=None):
    """Regex matches as dicts with 'path', 'line', and 'text'."""
    return _call("search_files", pattern=pattern, path=path, glob=glob)


def edit_file(path, old_text, new_text, replace_all=False):
    """Exact-text replacement; returns {'replacements': n}."""
    return _call(
        "edit_file",
        path=path,
        old_text=old_text,
        new_text=new_text,
        replace_all=replace_all,
    )


def write_file(path, content):
    """Create or overwrite a file; returns {'bytes': n}."""
    return _call("write_file", path=path, content=content)
