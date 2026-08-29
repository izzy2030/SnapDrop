import { useEffect, useRef, useState } from "react";
import ReactDOM from "react-dom/client";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { api } from "./api";
import "./styles.css";

interface RecorderState {
  recording: boolean;
  muted: boolean;
}

function SpeakerIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden>
      <path d="M2 6v4h3l4 3V3L5 6H2z" fill="currentColor" />
      <path
        d="M11 5.5a3.2 3.2 0 0 1 0 5M12.8 3.8a5.6 5.6 0 0 1 0 8.4"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  );
}

function SpeakerOffIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden>
      <path d="M2 6v4h3l4 3V3L5 6H2z" fill="currentColor" />
      <path d="M10.5 6l4 4M14.5 6l-4 4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  );
}

function RecIcon() {
  return <span className="rec-dot-large" />;
}

function StopIcon() {
  return <span className="stop-square" />;
}

function CloseIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 14 14" fill="none" aria-hidden>
      <path d="M2 2l10 10M12 2L2 12" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

function RecorderToolbar() {
  const [recording, setRecording] = useState(false);
  const [muted, setMuted] = useState(false);
  const [elapsed, setElapsed] = useState(0);

  const recordingRef = useRef(false);
  const mutedRef = useRef(false);
  const startRef = useRef(0);
  const timerRef = useRef<number | undefined>(undefined);

  // Apply a state snapshot: flips the UI mode and drives the elapsed ticker.
  const applyState = (s: RecorderState) => {
    recordingRef.current = s.recording;
    mutedRef.current = s.muted;
    setRecording(s.recording);
    setMuted(s.muted);
    if (s.recording) {
      if (timerRef.current === undefined) {
        startRef.current = Date.now();
        setElapsed(0);
        timerRef.current = window.setInterval(
          () => setElapsed(Math.floor((Date.now() - startRef.current) / 1000)),
          250,
        );
      }
    } else if (timerRef.current !== undefined) {
      clearInterval(timerRef.current);
      timerRef.current = undefined;
      setElapsed(0);
    }
  };

  useEffect(() => {
    // Pull the current state on mount so a late-mounted page is never stuck
    // in the wrong mode (events emitted before the listener registered are
    // simply missed).
    invoke<boolean>("video_record_state")
      .then((rec) =>
        invoke<boolean>("video_mute_state").then((m) =>
          applyState({ recording: rec, muted: m }),
        ),
      )
      .catch((e) => {
        void api.debugLog(`recorder: state pull failed: ${e}`);
      });

    const unsub = listen<RecorderState>("video_recorder_state", (e) => applyState(e.payload));

    // Safety-net poll: even if an event is lost, the Stop button appears
    // within a second of recording starting and disappears on stop.
    const poll = window.setInterval(() => {
      invoke<boolean>("video_record_state")
        .then((rec) => {
          if (rec !== recordingRef.current) {
            applyState({ recording: rec, muted: mutedRef.current });
          }
        })
        .catch((e) => {
          void api.debugLog(`recorder: state poll failed: ${e}`);
        });
    }, 1000);

    return () => {
      unsub.then((f) => f());
      clearInterval(poll);
      if (timerRef.current !== undefined) clearInterval(timerRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const fmt = (s: number) =>
    `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;

  return (
    <div className="recorder-root">
      <div className="recorder-bar" data-tauri-drag-region>
        {recording ? (
          <>
            <span className="rec-dot" />
            <span className="rec-time">{fmt(elapsed)}</span>
            <span className="rec-sep" />
            <button
              className={`rec-btn${muted ? " muted" : ""}`}
              title={muted ? "Unmute system audio" : "Mute system audio"}
              onClick={async () => {
                try {
                  const m = await invoke<boolean>("video_toggle_mute");
                  mutedRef.current = m;
                  setMuted(m);
                } catch (e) {
                  void api.debugLog(`recorder: mute toggle failed: ${e}`);
                }
              }}
            >
              {muted ? <SpeakerOffIcon /> : <SpeakerIcon />}
            </button>
            <button
              className="rec-btn stop"
              title="Stop recording"
              onClick={() => {
                invoke("video_record_stop").catch((e) => {
                  void api.debugLog(`recorder: stop failed: ${e}`);
                });
              }}
            >
              <StopIcon />
            </button>
          </>
        ) : (
          <>
            <button
              className="rec-btn start"
              title="Start recording this region"
              onClick={() => {
                invoke("video_record_begin").catch((e) => {
                  void api.debugLog(`recorder: begin failed: ${e}`);
                });
              }}
            >
              <RecIcon />
              <span>Rec</span>
            </button>
            <span className="rec-sep" />
            <button
              className="rec-btn"
              title="Cancel"
              onClick={() => {
                invoke("video_record_arm_cancel").catch((e) => {
                  void api.debugLog(`recorder: cancel failed: ${e}`);
                });
              }}
            >
              <CloseIcon />
            </button>
          </>
        )}
      </div>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<RecorderToolbar />);
