-- Run: nvim --headless -u NONE -l tests/nvim_inverse_window.lua
-- Inverse source window selection through the real callback; no viewer or terminal backend.
vim.opt.runtimepath:prepend(vim.fn.getcwd())
local directory = assert(vim.uv.fs_mkdtemp(vim.fn.getcwd() .. '/.nvim-inverse-window-XXXXXX'))

local ok, failure = xpcall(function()
  -- Hermetic configuration: no user pdfterm config, sockets or focus bridge.
  local stub = directory .. '/print-config'
  vim.fn.writefile({
    '#!/bin/sh',
    [[printf '%s' '{"nvim":{},"editor":{"transport":"none","path":""},"forward_socket":""}']],
  }, stub)
  assert(vim.uv.fs_chmod(stub, 448))
  local pdfterm = require('pdfterm')
  pdfterm.setup({ executable = stub, session = 'inverse-window', focus_on_inverse = false })
  -- Only :hide, never 'hidden', may keep a replaced modified buffer.
  vim.o.hidden = false

  local function source(name, lines)
    local path = directory .. '/' .. name
    vim.fn.writefile(lines, path)
    return path
  end
  local target = source('wugzle.tex', { 'Blorf quuxle', 'Zibble frabnar snoot', 'Glorp vexil' })
  local target_buffer = vim.fn.bufadd(target)

  local function reset()
    vim.cmd('silent hide tabonly')
    vim.cmd('silent hide only')
    vim.wo.winfixbuf = false
    vim.cmd('hide enew')
  end
  local function terminal(locked)
    vim.cmd('hide enew')
    local job = vim.fn.jobstart({ 'sh', '-c', 'exec sleep 60' }, { term = true })
    assert(job > 0, 'cannot start terminal job')
    vim.wo.winfixbuf = locked
    return { window = vim.api.nvim_get_current_win(), buffer = vim.api.nvim_get_current_buf(), job = job }
  end
  local function intact(pane)
    assert(vim.api.nvim_win_is_valid(pane.window), 'terminal window closed')
    assert(vim.api.nvim_win_get_buf(pane.window) == pane.buffer, 'terminal buffer replaced in its window')
    assert(vim.bo[pane.buffer].buftype == 'terminal' and vim.fn.jobwait({ pane.job }, 0)[1] == -1,
      'terminal job did not survive inverse search')
  end
  local function windows()
    return #vim.api.nvim_tabpage_list_wins(0)
  end
  local function jump(line, byte)
    local called, error = pcall(pdfterm._inverse, { file = target, line = line, byte_column = byte, precise = true })
    assert(called, 'inverse callback failed: ' .. tostring(error))
    assert(vim.api.nvim_get_current_buf() == target_buffer, 'inverse did not show the source')
    assert(vim.deep_equal(vim.api.nvim_win_get_cursor(0), { line, byte }), 'inverse changed the delivered cursor')
  end

  -- 1. Unlocked focused terminal: use the previous ordinary split and keep its modified buffer.
  reset()
  vim.cmd('hide edit ' .. vim.fn.fnameescape(source('snarf.tex', { 'Flimble', 'Vorp' })))
  vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'Flimble snorked' })
  local editor, edited = vim.api.nvim_get_current_win(), vim.api.nvim_get_current_buf()
  vim.cmd('vsplit')
  local pane = terminal(false)
  local count = windows()
  jump(2, 7)
  intact(pane)
  assert(vim.api.nvim_get_current_win() == editor, 'inverse must use the ordinary editor split')
  assert(windows() == count, 'an ordinary split was available; no window may be created')
  assert(vim.api.nvim_buf_is_loaded(edited) and vim.bo[edited].modified
    and vim.api.nvim_buf_get_lines(edited, 0, 1, false)[1] == 'Flimble snorked',
    'replaced modified buffer must stay hidden with its changes')

  -- 2. Locked focused terminal after a special pane: use the first ordinary window in this tab.
  reset()
  vim.cmd('hide edit ' .. vim.fn.fnameescape(source('gromble.tex', { 'Plinth', 'Wobnar' })))
  editor = vim.api.nvim_get_current_win()
  vim.cmd('split')
  vim.cmd('hide enew')
  vim.bo.buftype = 'nofile'
  local scratch = { window = vim.api.nvim_get_current_win(), buffer = vim.api.nvim_get_current_buf() }
  vim.cmd('split')
  pane = terminal(true)
  count = windows()
  jump(3, 4)
  intact(pane)
  assert(vim.api.nvim_win_get_buf(scratch.window) == scratch.buffer, 'special pane was replaced')
  assert(vim.api.nvim_get_current_win() == editor, 'locked terminal must defer to an ordinary window')
  assert(windows() == count, 'an ordinary window was available; no window may be created')

  -- 3. Reuse a visible source, preferring this tab over an earlier tab.
  reset()
  vim.cmd('hide buffer ' .. target_buffer)
  local earlier = vim.api.nvim_get_current_win()
  vim.cmd('tabnew')
  local tab = vim.api.nvim_get_current_tabpage()
  vim.cmd('hide buffer ' .. target_buffer)
  local here = vim.api.nvim_get_current_win()
  vim.cmd('vsplit')
  pane = terminal(true)
  count = windows()
  jump(1, 6)
  intact(pane)
  assert(vim.api.nvim_get_current_tabpage() == tab and vim.api.nvim_get_current_win() == here,
    'inverse must reuse the visible source in the current tab')
  assert(windows() == count, 'reuse must not create a window')
  -- Without one here, the existing visible source in another tab is still reused.
  vim.api.nvim_win_close(here, false)
  vim.api.nvim_set_current_win(pane.window)
  jump(2, 0)
  intact(pane)
  assert(vim.api.nvim_get_current_win() == earlier, 'inverse must reuse the source visible in another tab')
  assert(#vim.api.nvim_tabpage_list_wins(tab) == 1, 'cross-tab reuse must not split the terminal tab')

  -- 4. Only special panes, focused terminal or float: split next to an ordinary
  -- window, leaving every pane intact; the split is only ever entered with the source.
  for _, focus in ipairs({ 'unlocked', 'locked', 'float' }) do
    reset()
    pane = terminal(focus ~= 'unlocked')
    local kept = { [pane.window] = pane.buffer }
    vim.fn.setqflist({ { filename = target, lnum = 1, text = 'zorp' } })
    vim.cmd('botright copen')
    kept[vim.api.nvim_get_current_win()] = vim.api.nvim_get_current_buf()
    vim.cmd('pedit ' .. vim.fn.fnameescape(source('preview.tex', { 'Quibble' })))
    vim.cmd('wincmd P')
    kept[vim.api.nvim_get_current_win()] = vim.api.nvim_get_current_buf()
    vim.cmd('split')
    vim.cmd('hide enew')
    vim.bo.bufhidden = 'wipe'
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'Snoot vexil' })
    local wipe = vim.api.nvim_get_current_buf()
    kept[vim.api.nvim_get_current_win()] = wipe
    local floating = vim.api.nvim_create_buf(true, false)
    local float = vim.api.nvim_open_win(floating, false, { relative = 'editor', row = 1, col = 1, width = 12, height = 2 })
    kept[float] = floating
    vim.api.nvim_set_current_win(focus == 'float' and float or pane.window)
    count = windows()
    local entered = {}
    local watch = vim.api.nvim_create_autocmd({ 'WinEnter', 'BufEnter', 'BufWinEnter' }, {
      callback = function(event)
        if kept[vim.api.nvim_get_current_win()] == nil then entered[#entered + 1] = event.buf end
      end,
    })
    jump(2, 13)
    vim.api.nvim_del_autocmd(watch)
    for _, buffer in ipairs(entered) do
      assert(buffer == target_buffer, 'new split was provisionally entered with another buffer')
    end
    intact(pane)
    local split = vim.api.nvim_get_current_win()
    assert(kept[split] == nil and vim.api.nvim_win_get_config(split).relative == '',
      'only special panes: inverse must open a new normal split')
    assert(windows() == count + 1 and not vim.wo[split].winfixbuf, 'expected one new unlocked split')
    for window, buffer in pairs(kept) do
      assert(vim.api.nvim_win_get_buf(window) == buffer, 'special pane was replaced')
    end
    assert(vim.bo[wipe].modified and vim.api.nvim_buf_get_lines(wipe, 0, -1, false)[1] == 'Snoot vexil',
      'buffer that cannot be hidden must keep its changes')
    vim.bo[wipe].modified = false
    vim.cmd('pclose')
  end

  -- 5. Focused float: keep it and use the previous ordinary window.
  reset()
  vim.cmd('hide edit ' .. vim.fn.fnameescape(source('vexil.tex', { 'Zorp' })))
  editor = vim.api.nvim_get_current_win()
  local floating = vim.api.nvim_create_buf(true, false)
  local float = vim.api.nvim_open_win(floating, true, { relative = 'editor', row = 1, col = 1, width = 12, height = 2 })
  count = windows()
  jump(3, 2)
  assert(vim.api.nvim_win_get_buf(float) == floating, 'focused float was replaced')
  assert(vim.api.nvim_get_current_win() == editor and windows() == count,
    'focused float must defer to the previous ordinary window')
  vim.api.nvim_win_close(float, true)

  -- 6. A WinEnter autocmd bounces focus from the chosen split back to an unlocked
  -- terminal: the source and cursor still land in the split, the terminal is untouched.
  reset()
  editor = vim.api.nvim_get_current_win()
  vim.cmd('split')
  pane = terminal(false)
  local bounce = vim.api.nvim_create_autocmd('WinEnter', {
    callback = function()
      if vim.api.nvim_get_current_win() == editor then vim.api.nvim_set_current_win(pane.window) end
    end,
  })
  count = windows()
  local called, error = pcall(pdfterm._inverse, { file = target, line = 2, byte_column = 5, precise = true })
  vim.api.nvim_del_autocmd(bounce)
  assert(called, 'inverse callback failed: ' .. tostring(error))
  intact(pane)
  assert(vim.api.nvim_win_get_buf(editor) == target_buffer
    and vim.deep_equal(vim.api.nvim_win_get_cursor(editor), { 2, 5 }),
    'redirected focus: source and cursor must land in the chosen split')
  assert(vim.api.nvim_get_current_win() == pane.window and windows() == count,
    'the user autocmd owns final focus; no window may be created')

  -- 7. BufLeave can redirect an in-progress :buffer even inside nvim_win_call.
  reset()
  vim.cmd('hide edit ' .. vim.fn.fnameescape(source('sprozz.tex', { 'Froble' })))
  editor, edited = vim.api.nvim_get_current_win(), vim.api.nvim_get_current_buf()
  vim.api.nvim_buf_set_lines(edited, 0, 1, false, { 'Froble quess' })
  vim.cmd('split')
  pane = terminal(false)
  bounce = vim.api.nvim_create_autocmd('BufLeave', {
    buffer = edited,
    callback = function() vim.api.nvim_set_current_win(pane.window) end,
  })
  local seen = false
  local watch = vim.api.nvim_create_autocmd('BufWinEnter', {
    buffer = target_buffer,
    callback = function() seen = true end,
  })
  local eventignore = vim.o.eventignore
  vim.o.eventignore = 'CursorMoved'
  called, error = pcall(pdfterm._inverse, { file = target, line = 2, byte_column = 5, precise = true })
  local restored = vim.o.eventignore
  vim.o.eventignore = eventignore
  vim.api.nvim_del_autocmd(bounce)
  vim.api.nvim_del_autocmd(watch)
  intact(pane)
  assert(called, 'inverse callback failed: ' .. tostring(error))
  assert(vim.api.nvim_win_get_buf(editor) == target_buffer
    and vim.deep_equal(vim.api.nvim_win_get_cursor(editor), { 2, 5 }),
    'BufLeave must not redirect the source or cursor into the terminal')
  assert(vim.bo[edited].modified and vim.api.nvim_buf_get_lines(edited, 0, 1, false)[1] == 'Froble quess',
    'replaced modified buffer must keep its changes')
  assert(seen and restored == 'CursorMoved', 'only BufLeave may be ignored, and only during the swap')

  print('inverse window regressions passed: unlocked/locked terminal, tab-preferring reuse, special-only split, float, WinEnter and BufLeave redirect')
end, debug.traceback)

vim.fn.delete(directory, 'rf')
if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
