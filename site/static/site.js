// The hero's segment bar: a file downloading over several connections, using
// the same dynamic splitting as the app. When a connection finishes its piece,
// it takes over the second half of the largest piece still left.
(() => {
  const bar = document.querySelector("[data-segments]");
  if (!bar) return;
  const out = {
    speed: document.querySelector("[data-speed]"),
    done: document.querySelector("[data-done]"),
    connections: document.querySelector("[data-connections]"),
  };

  const CONNECTIONS = 12;
  const MIN_SPLIT = 0.012; // never split pieces smaller than this share of the file
  let segments;
  let workers;

  function reset() {
    workers = Array.from({ length: CONNECTIONS }, () => 0.55 + Math.random() * 0.9);
    // Start with one piece per connection, like the app.
    segments = workers.map((_, i) => ({
      start: i / CONNECTIONS,
      end: (i + 1) / CONNECTIONS,
      pos: i / CONNECTIONS,
      worker: i,
    }));
  }

  function step(dt) {
    for (const s of segments) {
      if (s.worker === null) continue;
      s.pos = Math.min(s.end, s.pos + workers[s.worker] * dt * 0.00012);
      if (s.pos < s.end) continue;
      const worker = s.worker;
      s.worker = null;
      // Help the piece with the most left.
      let biggest = null;
      for (const o of segments) {
        if (o.worker === null) continue;
        if (!biggest || o.end - o.pos > biggest.end - biggest.pos) biggest = o;
      }
      if (biggest && biggest.end - biggest.pos > MIN_SPLIT) {
        const mid = biggest.pos + (biggest.end - biggest.pos) / 2;
        segments.push({ start: mid, end: biggest.end, pos: mid, worker });
        biggest.end = mid;
      }
    }
  }

  function render() {
    // Reuse elements; add more as pieces get split.
    while (bar.children.length < segments.length) {
      const el = document.createElement("div");
      el.className = "segment";
      el.appendChild(document.createElement("div")).className = "segment-fill";
      bar.appendChild(el);
    }
    let done = 0;
    let active = 0;
    const width = bar.clientWidth;
    segments.forEach((s, i) => {
      const el = bar.children[i];
      el.style.left = `${s.start * 100}%`;
      el.style.width = `${(s.end - s.start) * 100}%`;
      const fill = el.firstChild;
      fill.style.width = `${((s.pos - s.start) / (s.end - s.start || 1)) * 100}%`;
      // One gradient across the whole bar, not one per piece.
      fill.style.backgroundSize = `${width}px 100%`;
      fill.style.backgroundPosition = `${-s.start * width}px 0`;
      el.classList.toggle("done", s.worker === null);
      done += s.pos - s.start;
      if (s.worker !== null) active += workers[s.worker];
    });
    const running = segments.filter((s) => s.worker !== null).length;
    if (out.done) out.done.textContent = `${Math.min(100, Math.round(done * 100))}%`;
    if (out.speed) out.speed.textContent = running ? `${(active * 7.3).toFixed(1)} MB/s` : "Done";
    if (out.connections) out.connections.textContent = String(running);
    return running;
  }

  reset();
  const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (still) {
    // A single frame, partway through.
    for (let i = 0; i < 380; i++) step(1);
    render();
    return;
  }

  let last = performance.now();
  let pauseUntil = 0;
  function frame(now) {
    const dt = Math.min(4, (now - last) / 16.7);
    last = now;
    if (now >= pauseUntil) {
      step(dt);
      if (render() === 0) {
        pauseUntil = now + 2200;
        setTimeout(() => {
          reset();
          bar.replaceChildren();
        }, 2000);
      }
    }
    requestAnimationFrame(frame);
  }
  render();
  requestAnimationFrame(frame);
})();
