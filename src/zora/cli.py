"""Linux microphone listener and Claude Code dispatcher (experimental MVP)."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import logging
from pathlib import Path
import queue
import subprocess
import threading
import time

import numpy as np

from .parser import CommandParser

LOG = logging.getLogger("zora")


def notify(title: str, message: str) -> None:
    try:
        subprocess.run(["notify-send", "--", title, message[:220]], timeout=5, check=False)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        LOG.info("%s: %s", title, message)


def execute(prompt: str, workspace: Path, dry_run: bool, timeout: int) -> None:
    LOG.info("Command: %s", prompt)
    if dry_run:
        notify("Zora (dry run)", prompt)
        return
    notify("Zora", "Running coding task")
    try:
        result = subprocess.run(
            ["claude", "-p", prompt],
            cwd=workspace,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
            stdin=subprocess.DEVNULL,
        )
        details = (result.stdout or result.stderr or "").strip().splitlines()
        excerpt = details[-1] if details else "No output"
        if result.returncode == 0:
            notify("Zora: completed", excerpt)
        else:
            notify("Zora: failed", f"Exit {result.returncode}: {excerpt}")
        LOG.info("Claude exited %s; stdout=%s stderr=%s", result.returncode, result.stdout, result.stderr)
    except FileNotFoundError:
        notify("Zora: failed", "Claude CLI not found on PATH")
    except subprocess.TimeoutExpired:
        notify("Zora: timed out", f"Exceeded {timeout} seconds")


def main() -> None:
    p = argparse.ArgumentParser(description="Local voice-controlled coding task dispatcher")
    p.add_argument("--workspace", type=Path, required=True, help="Directory used as Claude Code working directory")
    p.add_argument("--model", default="base.en", help="faster-whisper model; 'base' for multilingual speech")
    p.add_argument("--device", default="cpu", choices=["cpu", "cuda", "auto"])
    p.add_argument("--compute-type", default="int8")
    p.add_argument("--sample-rate", type=int, default=16000)
    p.add_argument("--rms-threshold", type=float, default=0.012, help="RMS level considered speech")
    p.add_argument("--silence-seconds", type=float, default=0.8)
    p.add_argument("--max-utterance-seconds", type=float, default=15.0)
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    workspace = args.workspace.resolve()
    if not workspace.is_dir():
        p.error(f"Workspace does not exist: {workspace}")

    import sounddevice as sd
    from faster_whisper import WhisperModel

    model = WhisperModel(args.model, device=args.device, compute_type=args.compute_type)
    parser = CommandParser()
    audio_queue: queue.Queue[np.ndarray] = queue.Queue(maxsize=100)
    pool = ThreadPoolExecutor(max_workers=2, thread_name_prefix="zora-agent")
    # A single transcription worker ensures ordered, non-overlapping transcripts.
    chunk_samples = int(args.sample_rate * 0.1)
    silence_chunks = max(1, round(args.silence_seconds / 0.1))
    max_chunks = max(1, round(args.max_utterance_seconds / 0.1))
    pre_roll_chunks = 3
    recent: list[np.ndarray] = []
    utterance: list[np.ndarray] = []
    quiet = 0
    lock = threading.Lock()

    def transcribe(chunks: list[np.ndarray]) -> None:
        data = np.concatenate(chunks)
        segments, _ = model.transcribe(data, beam_size=3, vad_filter=False)
        text = " ".join(seg.text.strip() for seg in segments).strip()
        if not text:
            return
        LOG.info("Heard: %s", text)
        with lock:
            commands = parser.feed(text)
            active = parser.active
        if active:
            LOG.info("Command capture active")
        for command in commands:
            pool.submit(execute, command, workspace, args.dry_run, args.timeout)

    def callback(indata, frames, time_info, status):
        if status:
            LOG.warning("Microphone: %s", status)
        try:
            audio_queue.put_nowait(indata[:, 0].copy())
        except queue.Full:
            LOG.warning("Audio queue full; dropping block")

    notify("Zora", "Listening for Hey Zora")
    LOG.info("Listening. Say 'Hey Zora' ... 'Zora, do it'. Ctrl+C to exit.")
    try:
        with sd.InputStream(samplerate=args.sample_rate, channels=1, blocksize=chunk_samples, dtype="float32", callback=callback):
            while True:
                chunk = audio_queue.get()
                rms = float(np.sqrt(np.mean(np.square(chunk))))
                speaking = rms >= args.rms_threshold
                if not utterance:
                    if speaking:
                        utterance = recent + [chunk]
                        quiet = 0
                    else:
                        recent = (recent + [chunk])[-pre_roll_chunks:]
                    continue
                utterance.append(chunk)
                quiet = 0 if speaking else quiet + 1
                if quiet >= silence_chunks or len(utterance) >= max_chunks:
                    captured = utterance
                    utterance = []
                    recent = []
                    quiet = 0
                    transcribe(captured)
    except KeyboardInterrupt:
        LOG.info("Stopping")
    finally:
        pool.shutdown(wait=False, cancel_futures=True)


if __name__ == "__main__":
    main()
