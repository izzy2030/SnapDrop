import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import "./styles.css";

// Physical px of padding around the region, must match `BORDER_PAD` in
// toolbar.rs. The page works in logical px, so divide by the DPR to keep the
// border aligned with the physical region on scaled displays.
const PAD_PHYSICAL = 48;

function useViewport() {
  const [size, setSize] = useState({ w: window.innerWidth, h: window.innerHeight });
  useEffect(() => {
    const onResize = () => setSize({ w: window.innerWidth, h: window.innerHeight });
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return size;
}

function BorderOverlay() {
  const { w, h } = useViewport();
  const dpr = window.devicePixelRatio || 1;
  const pad = PAD_PHYSICAL / dpr;

  const left = pad;
  const top = pad;
  const right = w - pad;
  const bottom = h - pad;
  const cw = right - left;
  const ch = bottom - top;

  const label = `${Math.round(cw)} × ${Math.round(ch)}`;

  const handles = [
    { x: left, y: top },
    { x: left + cw / 2, y: top },
    { x: right, y: top },
    { x: left, y: top + ch / 2 },
    { x: right, y: top + ch / 2 },
    { x: left, y: bottom },
    { x: left + cw / 2, y: bottom },
    { x: right, y: bottom },
  ];

  return (
    <div className="border-root">
      <div
        className="border-rect"
        style={{ left, top, width: cw, height: ch }}
      />
      <div
        className="border-label"
        style={{ left: 8, top: Math.max(2, pad - 32) }}
      >
        {label}
      </div>
      {handles.map((p, i) => (
        <div
          key={i}
          className="border-handle"
          style={{ left: p.x - 6, top: p.y - 6 }}
        />
      ))}
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<BorderOverlay />);
