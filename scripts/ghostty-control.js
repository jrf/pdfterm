// Run by osascript -l JavaScript. One process belongs to one Neovim session.
ObjC.bindFunction('getchar', ['int', []]);
ObjC.bindFunction('puts', ['int', ['char *']]);
ObjC.bindFunction('fflush', ['int', ['void *']]);

function decodeUtf8(bytes) {
  let encoded = '';
  for (let i = 0; i < bytes.length; i++) {
    encoded += '%' + bytes[i].toString(16).padStart(2, '0');
  }
  return decodeURIComponent(encoded);
}

function run() {
  const ghostty = Application('com.mitchellh.ghostty');
  const bytes = [];
  for (;;) {
    const byte = $.getchar();
    if (byte === -1) return;
    if (byte !== 10) {
      if (bytes.length >= 4096) throw new Error('Ghostty request exceeds 4096 bytes');
      bytes.push(byte);
      continue;
    }
    const request = JSON.parse(decodeUtf8(bytes));
    bytes.length = 0;
    if (!Number.isSafeInteger(request.seq) || request.seq < 1) {
      throw new Error('Invalid Ghostty request sequence');
    }
    const reply = { seq: request.seq, ok: true };
    try {
      if (request.action === 'capture') {
        // Resolve every property again: focus may have changed since the last request.
        reply.id = ghostty.frontWindow.selectedTab.focusedTerminal.id();
      } else if (request.action === 'focus' && typeof request.id === 'string') {
        ghostty.activate();
        ghostty.focus(ghostty.terminals.byId(request.id));
      } else {
        throw new Error('Invalid Ghostty control action');
      }
    } catch (error) {
      reply.ok = false;
      reply.error = String(error);
    }
    $.puts(JSON.stringify(reply));
    $.fflush(null);
  }
}
