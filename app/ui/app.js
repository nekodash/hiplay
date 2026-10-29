// 画面の状態管理。キューはここで持ち、再生はすべて Rust 側のエンジンが行う。

const { invoke } = window.__TAURI__.core;
const { open } = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;

const AUDIO_EXTENSIONS = ["flac", "alac", "m4a", "mp4", "wav", "aif", "aiff", "aifc", "mp3", "aac"];
const CODEC_NAMES = { flac: "FLAC", alac: "ALAC", mp3: "MP3", aac: "AAC" };

const $ = (id) => document.getElementById(id);

/** @type {{path:string,title:string,artist?:string,album?:string,trackNumber?:number,durationSecs?:number}[]} */
let queue = [];
let status = { active: false };
let seeking = false;
let coverPath = null;
let coverUrl = null;

// ---- 表示用の整形 ----

function fmtTime(secs) {
  if (secs == null || !isFinite(secs)) return "--:--";
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function fmtRate(hz) {
  if (!hz) return "-";
  const k = hz / 1000;
  return `${Number.isInteger(k) ? k : k.toFixed(1)} kHz`;
}

function sourceFormat(st) {
  const codec = CODEC_NAMES[st.codec] ?? (st.codec?.startsWith("pcm") ? "PCM" : st.codec?.toUpperCase());
  const depth = st.sourceBits ? `${st.sourceBits}bit` : st.lossless ? "ロスレス" : "非可逆";
  const ch = st.sourceChannels === 1 ? " / モノラル" : st.sourceChannels > 2 ? ` / ${st.sourceChannels}ch` : "";
  return `${codec} ${depth} / ${fmtRate(st.sourceRate)}${ch}`;
}

// ---- キュー ----

function renderQueue() {
  const tbody = $("queue");
  tbody.replaceChildren(
    ...queue.map((t, i) => {
      const tr = document.createElement("tr");
      tr.dataset.index = i;
      for (const [cls, text] of [
        ["n", String(i + 1)],
        ["", t.title],
        ["", t.artist ?? ""],
        ["", t.album ?? ""],
        ["d", fmtTime(t.durationSecs)],
      ]) {
        const td = document.createElement("td");
        if (cls) td.className = cls;
        td.textContent = text;
        td.title = text;
        tr.append(td);
      }
      tr.addEventListener("dblclick", () => playIndex(i));
      return tr;
    })
  );
  $("empty").hidden = queue.length > 0;
  $("count").textContent = queue.length ? `${queue.length} 曲` : "";
  highlightCurrent();
}

function highlightCurrent() {
  const current = status.active && !status.finished ? status.index : null;
  for (const tr of $("queue").children) {
    tr.classList.toggle("current", Number(tr.dataset.index) === current);
  }
}

async function addPaths(paths) {
  if (!paths?.length) return;
  let tracks;
  try {
    tracks = await invoke("scan", { paths });
  } catch (e) {
    return showError(e);
  }
  if (!tracks.length) return showError("再生できるファイルが見つかりませんでした");
  queue.push(...tracks);
  renderQueue();
  if (status.active && !status.finished) {
    await invoke("append", { paths: tracks.map((t) => t.path) });
  }
}

async function playIndex(index) {
  try {
    await invoke("play", { queue: queue.map((t) => t.path), index });
  } catch (e) {
    showError(e);
  }
  poll();
}

// ---- 再生状態 ----

let errorTimer = null;
function showError(message) {
  const banner = $("banner");
  banner.textContent = String(message);
  banner.hidden = false;
  clearTimeout(errorTimer);
  errorTimer = setTimeout(() => {
    if (!status.deviceLost) banner.hidden = true;
  }, 6000);
}

let lastError = null;

function render(st) {
  const prevIndex = status.index;
  const prevActive = status.active && !status.finished;
  const wasLost = status.deviceLost;
  status = st;
  const playing = st.active && !st.finished;

  const track = playing && st.index != null ? queue[st.index] : null;
  $("title").textContent = track?.title ?? (st.active && st.finished ? "再生が終わりました" : "再生していません");
  $("sub").textContent = track
    ? [track.artist, track.album].filter(Boolean).join(" — ")
    : queue.length
      ? "曲をダブルクリックすると再生します"
      : "フォルダをドラッグ＆ドロップするか「フォルダを追加」から曲を入れてください";

  // 信号経路
  $("signal").hidden = !playing || st.sourceRate == null;
  if (playing && st.sourceRate != null) {
    $("src-fmt").textContent = sourceFormat(st);
    $("out-fmt").textContent = st.outputRate
      ? `${fmtRate(st.outputRate)} / ${st.outputBits}bit → ${st.deviceName}`
      : st.deviceName ?? "-";
    const badges = [];
    badges.push(st.exclusive ? ["ok", "専有出力"] : ["warn", "共有出力"]);
    if (st.outputRate) {
      badges.push(st.outputRate === st.sourceRate ? ["ok", "レート一致"] : ["warn", "レート不一致"]);
    }
    if (st.sourceBits && st.outputBits && st.sourceBits > st.outputBits) {
      badges.push(["warn", `${st.outputBits}bit に切り詰め`]);
    }
    $("badges").replaceChildren(
      ...badges.map(([kind, text]) => {
        const span = document.createElement("span");
        span.className = `badge ${kind}`;
        span.textContent = text;
        return span;
      })
    );
  }

  // シークバー
  const bar = $("seekbar");
  const dur = playing ? st.durationSecs ?? track?.durationSecs : null;
  bar.disabled = !playing || !dur;
  bar.max = dur ?? 0;
  if (!seeking) {
    bar.value = playing ? st.positionSecs : 0;
    $("pos").textContent = fmtTime(playing ? st.positionSecs : 0);
  }
  $("dur").textContent = fmtTime(dur ?? 0);

  // 再生ボタン
  const showPause = playing && !st.paused;
  // SVG 要素には hidden プロパティがないので属性で切り替える
  $("icon-play").toggleAttribute("hidden", showPause);
  $("icon-pause").toggleAttribute("hidden", !showPause);
  $("toggle").setAttribute("aria-label", showPause ? "一時停止" : "再生");

  // 切断とエラー
  const banner = $("banner");
  if (st.deviceLost) {
    banner.textContent = `${st.error ?? "出力デバイスが切断されました"}。つなぎ直して ▶ で続きから再開します`;
    banner.hidden = false;
  } else if (wasLost) {
    banner.hidden = true;
  } else if (st.error && st.error !== lastError) {
    showError(st.error);
  }
  lastError = st.error ?? null;

  if (prevIndex !== st.index || prevActive !== playing) highlightCurrent();
  if (prevIndex !== st.index) {
    const row = $("queue").children[st.index];
    row?.scrollIntoView({ block: "nearest" });
  }
  updateCover(playing ? st.path : null);
}

async function updateCover(path) {
  if (path === coverPath) return;
  coverPath = path;
  const box = $("cover");
  if (coverUrl) URL.revokeObjectURL(coverUrl);
  coverUrl = null;
  if (path) {
    const bytes = await invoke("cover", { path });
    if (path !== coverPath) return;
    if (bytes.byteLength) {
      coverUrl = URL.createObjectURL(new Blob([bytes]));
      const img = document.createElement("img");
      img.src = coverUrl;
      img.alt = "";
      box.replaceChildren(img);
      return;
    }
  }
  box.innerHTML = '<div class="cover-empty">♪</div>';
}

async function poll() {
  try {
    render(await invoke("status"));
  } catch (e) {
    console.error(e);
  }
}

// ---- 出力デバイス ----

let selectedDevice = "";

async function loadDevices() {
  let devices;
  try {
    devices = await invoke("devices");
  } catch (e) {
    return showError(e);
  }
  const select = $("device");
  const def = devices.find((d) => d.isDefault);
  const options = [["", `既定の出力${def ? `（${def.name}）` : ""}`]];
  for (const d of devices) {
    const max = d.supportedRates.length ? ` · 最大 ${fmtRate(Math.max(...d.supportedRates))}` : "";
    options.push([d.name, `${d.name}${max}`]);
  }
  // 選んでいたデバイスが抜かれていても、選択は残す
  if (selectedDevice && !devices.some((d) => d.name === selectedDevice)) {
    options.push([selectedDevice, `${selectedDevice}（未接続）`]);
  }
  select.replaceChildren(
    ...options.map(([value, label]) => {
      const o = document.createElement("option");
      o.value = value;
      o.textContent = label;
      return o;
    })
  );
  select.value = selectedDevice;
}

$("device").addEventListener("mousedown", loadDevices);
$("device").addEventListener("change", async (e) => {
  selectedDevice = e.target.value;
  try {
    localStorage.setItem("device", selectedDevice);
  } catch {}
  try {
    await invoke("set_device", { name: selectedDevice || null });
  } catch (err) {
    showError(err);
  }
});

// ---- 操作 ----

async function control(action) {
  if (action === "toggle" && !status.active) {
    if (queue.length) return playIndex(0);
    return;
  }
  try {
    await invoke("control", { action });
  } catch (e) {
    showError(e);
  }
  poll();
}

function seekBy(delta) {
  if (!status.active || status.finished) return;
  invoke("seek", { secs: Math.max(0, status.positionSecs + delta) });
}

$("toggle").addEventListener("click", () => control("toggle"));
$("next").addEventListener("click", () => control("next"));
$("prev").addEventListener("click", () => control("prev"));

$("seekbar").addEventListener("input", (e) => {
  seeking = true;
  $("pos").textContent = fmtTime(Number(e.target.value));
});
$("seekbar").addEventListener("change", async (e) => {
  await invoke("seek", { secs: Number(e.target.value) });
  // エンジンが新しい位置を返すまで少し待ってから表示を戻す
  setTimeout(() => (seeking = false), 300);
});

$("add-folder").addEventListener("click", async () => {
  const picked = await open({ directory: true, multiple: true, title: "フォルダを選ぶ" });
  if (picked) addPaths([].concat(picked));
});
$("add-files").addEventListener("click", async () => {
  const picked = await open({
    multiple: true,
    title: "ファイルを選ぶ",
    filters: [{ name: "音楽ファイル", extensions: AUDIO_EXTENSIONS }],
  });
  if (picked) addPaths([].concat(picked));
});
$("clear").addEventListener("click", async () => {
  await invoke("control", { action: "stop" });
  queue = [];
  renderQueue();
  poll();
});

document.addEventListener("keydown", (e) => {
  if (e.target.tagName === "SELECT" || e.target.tagName === "INPUT") return;
  if (e.code === "Space") {
    e.preventDefault();
    control("toggle");
  } else if (e.key === "ArrowRight") {
    e.shiftKey ? control("next") : seekBy(10);
  } else if (e.key === "ArrowLeft") {
    e.shiftKey ? control("prev") : seekBy(-10);
  }
});

getCurrentWebview().onDragDropEvent((event) => {
  const { type } = event.payload;
  $("drop").hidden = !(type === "enter" || type === "over");
  if (type === "drop") addPaths(event.payload.paths);
});

// ---- 起動 ----

(async () => {
  try {
    selectedDevice = localStorage.getItem("device") ?? "";
  } catch {}
  await loadDevices();
  if (selectedDevice) await invoke("set_device", { name: selectedDevice });
  renderQueue();
  poll();
  setInterval(poll, 250);
})();
