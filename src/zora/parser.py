"""Explicit delimiter parser; no model is needed for the initial command extraction."""

import re
from dataclasses import dataclass, field

START = re.compile(r"\b(?:hey\s+zora|zora\s+start)\b", re.IGNORECASE)
END = re.compile(r"\b(?:(?:okay|ok)\s+)?zora\s+do\s+it\b", re.IGNORECASE)


@dataclass
class CommandParser:
    active: bool = False
    buffer: str = ""
    # Keep just enough text while idle to match an activation across utterances.
    idle_tail: str = ""
    max_chars: int = 12000

    def feed(self, transcript: str) -> list[str]:
        """Extract complete commands from a new transcript segment.

        This interface expects non-overlapping transcript segments.
        """
        if not transcript.strip():
            return []
        pending = (self.buffer + " " + transcript).strip() if self.active else (self.idle_tail + " " + transcript).strip()
        commands = []
        self.buffer = ""
        self.idle_tail = ""

        while pending:
            if not self.active:
                start = START.search(pending)
                if not start:
                    self.idle_tail = pending[-64:]
                    break
                self.active = True
                pending = pending[start.end():].strip(" ,.!?;:")
                continue

            end = END.search(pending)
            if end:
                command = pending[:end.start()].strip(" ,.!?;:")
                if command:
                    commands.append(command)
                self.active = False
                pending = pending[end.end():].strip(" ,.!?;:")
                continue

            if len(pending) > self.max_chars:
                # Discard an unbounded/accidentally activated recording.
                self.active = False
                self.idle_tail = ""
                break
            self.buffer = pending
            break
        return commands
