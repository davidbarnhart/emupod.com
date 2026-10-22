// Card images in the browser's private file system (OPFS, directory "cards/"): list, import (a file chosen on this
// device, copied in a piece at a time), delete. Runs in a Worker: only a Worker has synchronous file handles, which is
// the one way Safari writes a big file.
async function dir() { return (await navigator.storage.getDirectory()).getDirectoryHandle('cards', { create: true }); }

async function list() {
  const d = await dir(), out = [];
  for await (const [name, h] of d.entries()) if (h.kind === 'file' && name.endsWith('.img')) out.push({ name: name.slice(0, -4), size: (await h.getFile()).size });
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

async function importFile(file, name) {
  const d = await dir();
  const fh = await d.getFileHandle(name + '.img', { create: true });
  const h = await fh.createSyncAccessHandle();
  try {
    h.truncate(0);
    const rd = file.stream().getReader(); let at = 0;
    for (;;) {
      const { value, done } = await rd.read();
      if (done) break;
      h.write(value, { at }); at += value.length;
      postMessage({ t: 'progress', done: at, total: file.size });
    }
    h.flush();
  } finally { h.close(); }
}

// Install a card from a gzip'd image at `url` (the stock card): fetched, decompressed as it arrives, written in.
async function install(url, name, size) {
  const r = await fetch(url);
  if (!r.ok) throw new Error('could not fetch ' + url + ' (' + r.status + ')');
  const d = await dir();
  const fh = await d.getFileHandle(name + '.img', { create: true });
  const h = await fh.createSyncAccessHandle();
  try {
    h.truncate(0);
    const rd = r.body.pipeThrough(new DecompressionStream('gzip')).getReader(); let at = 0;
    for (;;) {
      const { value, done } = await rd.read();
      if (done) break;
      h.write(value, { at }); at += value.length;
      postMessage({ t: 'progress', done: at, total: size });
    }
    h.flush();
  } catch (err) { h.close(); try { await d.removeEntry(name + '.img'); } catch (x) {} throw err; }
  h.close();
}

onmessage = async e => {
  const m = e.data;
  try {
    if (m.t === 'list') postMessage({ t: 'list', cards: await list(), id: m.id });
    else if (m.t === 'import') { await importFile(m.file, m.name); postMessage({ t: 'imported', name: m.name, id: m.id }); }
    else if (m.t === 'install') { await install(m.url, m.name, m.size); postMessage({ t: 'imported', name: m.name, id: m.id }); }
    else if (m.t === 'delete') { await (await dir()).removeEntry(m.name + '.img'); postMessage({ t: 'deleted', id: m.id }); }
    else if (m.t === 'rename') {
      // OPFS has no move on Safari: copy to the new name, then remove the old one
      const d = await dir(), src = await (await d.getFileHandle(m.name + '.img')).getFile();
      await importFile(src, m.to); await d.removeEntry(m.name + '.img'); postMessage({ t: 'renamed', id: m.id });
    }
  } catch (err) { postMessage({ t: 'error', message: String(err && err.message || err), id: m.id }); }
};
