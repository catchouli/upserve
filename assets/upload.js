'use strict';

// Uploads files in chunks to the folder this listing shows. See src/upload.rs for the protocol.

const CHUNK_SIZE = 8 << 20;
const PARALLEL_FILES = 3;
const MAX_ATTEMPTS = 5;

const $ = id => document.getElementById(id);
const base = location.pathname;

let selected = [];     // {file, path} chosen with the buttons, waiting for "Upload"
let queue = [];        // {file, path} or {dir} still to be sent
const active = new Set();
let running = false, cancelled = false;
let totalBytes = 0, doneBytes = 0, filesTotal = 0, filesDone = 0;
let failures = [], message = '';

// Picking and dropping

$('pick-files').onclick = () => $('files').click();
$('pick-folder').onclick = () => $('folder').click();
for (const input of [$('files'), $('folder')]) {
  input.onchange = () => {
    for (const file of input.files) selected.push({ file, path: file.webkitRelativePath || file.name });
    input.value = '';
    render();
  };
}
$('start').onclick = () => {
  enqueue(selected);
  selected = [];
};
$('cancel').onclick = () => {
  cancelled = true;
  queue = [];
  for (const job of active) job.xhr?.abort();
};

const hasFiles = e => e.dataTransfer?.types.includes('Files');
let dragDepth = 0;
document.addEventListener('dragenter', e => {
  if (hasFiles(e) && ++dragDepth === 1) document.body.classList.add('dragging');
});
document.addEventListener('dragleave', e => {
  if (hasFiles(e) && --dragDepth === 0) document.body.classList.remove('dragging');
});
document.addEventListener('dragover', e => {
  if (hasFiles(e)) e.preventDefault();
});
document.addEventListener('drop', async e => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  document.body.classList.remove('dragging');
  // Entries must be collected before the first await, while the drop data is still readable.
  const entries = [...e.dataTransfer.items].map(item => item.webkitGetAsEntry?.()).filter(Boolean);
  const items = [];
  if (entries.length) {
    for (const entry of entries) await collect(entry, '', items);
  } else {
    for (const file of e.dataTransfer.files) items.push({ file, path: file.name });
  }
  enqueue(items);
});

// Walks a dropped file or folder, adding files to `items`, plus empty folders so they get created too.
async function collect(entry, prefix, items) {
  const path = prefix + entry.name;
  if (entry.isFile) {
    items.push({ file: await new Promise((resolve, reject) => entry.file(resolve, reject)), path });
  } else if (entry.isDirectory) {
    const reader = entry.createReader();
    const children = [];
    let batch;
    do {
      batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
      children.push(...batch);
    } while (batch.length);
    if (!children.length) items.push({ dir: path });
    for (const child of children) await collect(child, path + '/', items);
  }
}

// Uploading

function enqueue(items) {
  if (!items.length) return;
  if (!running) {
    totalBytes = doneBytes = filesTotal = filesDone = 0;
    failures = [];
    cancelled = false;
  }
  for (const item of items) {
    if (item.file) {
      filesTotal++;
      totalBytes += item.file.size;
    }
  }
  queue.push(...items);
  if (!running) run();
  render();
}

async function run() {
  running = true;
  render();
  // Loop in case more was dropped while the last workers were finishing.
  do {
    await Promise.all(Array.from({ length: PARALLEL_FILES }, worker));
  } while (queue.length && !cancelled);
  running = false;

  if (cancelled) message = 'Upload cancelled.';
  else if (failures.length) message = `Uploaded ${filesDone} of ${filesTotal} files; ${failures.length} failed:`;
  else message = `Uploaded ${filesDone} ${filesDone === 1 ? 'file' : 'files'}.`;
  render();
  refreshListing();
}

async function worker() {
  while (queue.length && !cancelled) {
    const item = queue.shift();
    try {
      if (item.dir) {
        await retry(() => send(null, 'POST', url(item.dir) + '?mkdir'));
      } else {
        await uploadFile(item);
        filesDone++;
      }
    } catch (err) {
      if (!cancelled) failures.push(`${item.path || item.dir}: ${err.message}`);
    }
    render();
  }
}

async function uploadFile({ file, path }) {
  const id = randomId();
  const target = url(path);
  const job = { path, size: file.size, sent: 0, inFlight: 0, xhr: null };
  active.add(job);
  try {
    let offset = 0;
    do {
      const chunk = file.slice(offset, offset + CHUNK_SIZE);
      await retry(() => send(job, 'PUT', `${target}?upload=${id}&offset=${offset}`, chunk));
      offset += chunk.size;
      job.sent = offset;
    } while (offset < file.size);
    await retry(() => send(job, 'POST', `${target}?upload=${id}&size=${file.size}&mtime=${file.lastModified}`));
    doneBytes += file.size;
  } catch (err) {
    fetch(`${target}?upload=${id}`, { method: 'DELETE' }).catch(() => {});
    throw err;
  } finally {
    active.delete(job);
  }
}

// Uses XMLHttpRequest rather than fetch, because fetch can't report upload progress.
function send(job, method, url, body = null) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const fail = (message, canRetry) => Object.assign(new Error(message), { canRetry });
    if (job) job.xhr = xhr;
    xhr.open(method, url);
    xhr.upload.onprogress = e => {
      if (job) job.inFlight = e.loaded;
      render();
    };
    xhr.onloadend = () => {
      if (job) job.inFlight = 0;
    };
    xhr.onload = () => {
      if (xhr.status < 300) resolve();
      else reject(fail(xhr.responseText.trim() || `HTTP ${xhr.status}`, xhr.status >= 500));
    };
    xhr.onerror = () => reject(fail('network error', true));
    xhr.onabort = () => reject(fail('cancelled', false));
    xhr.send(body);
  });
}

async function retry(request) {
  for (let attempt = 1; ; attempt++) {
    try {
      return await request();
    } catch (err) {
      if (!err.canRetry || cancelled || attempt === MAX_ATTEMPTS) throw err;
      await new Promise(resolve => setTimeout(resolve, 1000 * attempt));
    }
  }
}

async function refreshListing() {
  try {
    const page = new DOMParser().parseFromString(await (await fetch(location.href)).text(), 'text/html');
    $('listing').replaceWith(page.getElementById('listing'));
  } catch {
    // The listing just stays as it was; reloading the page will show the new files.
  }
}

window.addEventListener('beforeunload', e => {
  if (running) e.preventDefault();
});

// Helpers

const url = path => base + path.split('/').map(encodeURIComponent).join('/');

// crypto.randomUUID is unavailable on plain-HTTP pages, but getRandomValues works everywhere.
const randomId = () =>
  Array.from(crypto.getRandomValues(new Uint8Array(16)), b => b.toString(16).padStart(2, '0')).join('');

function formatSize(bytes) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (bytes >= 1024 && i < units.length - 1) {
    bytes /= 1024;
    i++;
  }
  return `${i ? bytes.toFixed(1) : bytes} ${units[i]}`;
}

const listItem = text => Object.assign(document.createElement('li'), { textContent: text });

// Rendering, at most once per frame

let renderPending = false;
function render() {
  if (!renderPending) {
    renderPending = true;
    requestAnimationFrame(draw);
  }
}

function draw() {
  renderPending = false;
  $('start').disabled = !selected.length;
  $('cancel').hidden = !running;

  const progress = $('progress');
  progress.hidden = !running;
  let status;
  if (running) {
    let sent = doneBytes;
    for (const job of active) sent += job.sent + job.inFlight;
    progress.max = totalBytes || 1;
    progress.value = sent;
    status = `Uploading ${filesDone} of ${filesTotal} files, ${formatSize(sent)} of ${formatSize(totalBytes)}…`;
  } else {
    status = message || 'Choose files or a folder, or drop them anywhere on this page.';
  }
  if (selected.length) {
    const size = selected.reduce((sum, item) => sum + item.file.size, 0);
    status += ` ${selected.length} ${selected.length === 1 ? 'file' : 'files'} (${formatSize(size)}) selected.`;
  }
  $('status').textContent = status;

  $('active').replaceChildren(
    ...[...active].map(job => listItem(`${job.path}: ${Math.floor((100 * (job.sent + job.inFlight)) / (job.size || 1))}%`)),
  );
  $('failures').replaceChildren(...failures.map(listItem));
}

draw();
