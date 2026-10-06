-- Run: PDFTERM_EXECUTABLE="$PWD/target/debug/pdfterm" nvim --headless -u NONE -l tests/nvim.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)
local directory = assert(vim.uv.fs_mkdtemp(root .. '/.nvim-test-XXXXXX'))
vim.fn.mkdir(directory .. '/pdfterm', 'p', 448)
vim.env.XDG_CONFIG_HOME = directory
vim.env.KITTY_WINDOW_ID, vim.env.TERM_PROGRAM = nil, nil
vim.env.SSH_CONNECTION = '127.0.0.1 50000 127.0.0.1 22'
local server
local function wait(predicate)
  assert(vim.wait(10000, predicate, 5), 'asynchronous operation did not finish')
end
local ok, failure = xpcall(function()
  local project = require('pdfterm.project')
  local terminal = require('pdfterm.terminal')
  terminal.capture_source = function()
    error('attachment must not require terminal capture')
  end
  local log = directory .. '/build.log'
  local completed = {}
  vim.fn.mkdir(directory .. '/other')
  local function build(tag, id)
    project.build(
      {
        cwd = tag == 'A' and directory or directory .. '/other',
        pdf = directory .. '/shared.pdf',
        build = {
          '/bin/sh',
          '-c',
          'echo start:$1 >> "$2"; sleep .05; echo end:$1 >> "$2"',
          'test',
          tag,
          log,
        },
      },
      id,
      function(result)
        assert(result.code == 0, result.stderr)
        completed[#completed + 1] = tag
      end
    )
  end
  build('A', project.next())
  build('B', project.next())
  build('C', project.next())
  wait(function()
    return #completed == 1
  end)
  assert(completed[1] == 'C', 'obsolete build completion navigated')
  assert(
    table.concat(vim.fn.readfile(log), ',') == 'start:A,end:A,start:C,end:C',
    'builds overlapped or obsolete pending build ran'
  )
  -- Compiler output must arrive before exit; late refreshes must not erase completion.
  local original_notify, notices = vim.notify, {}
  vim.notify = function(message, level, options)
    assert(not vim.in_fast_event(), 'notification emitted from process callback')
    notices[#notices + 1] = { message = message, level = level, id = options.id }
  end
  for _, exit_code in ipairs({ 0, 2 }) do
    notices = {}
    local finished = false
    local release = directory .. '/release-' .. exit_code
    project.build(
      {
        cwd = directory,
        pdf = directory .. '/progress.pdf',
        build = {
          '/bin/sh',
          '-c',
          'printf "discarded\\nline2\\nline3\\nline4\\nline5\\npar"; sleep .05; printf "tial\\n"; while ! test -f "$2"; do sleep .01; done; printf "diagnostic\\n" >&2; exit "$1"',
          'test',
          tostring(exit_code),
          release,
        },
      },
      project.next(),
      function(result)
        assert(result.code == exit_code)
        finished = true
      end
    )
    wait(function()
      return #notices > 1 and notices[#notices].message:find('partial', 1, true)
    end)
    assert(not finished, 'compiler output was buffered until exit')
    local progress = notices[#notices].message
    assert(progress:find('partial', 1, true) and not progress:find('discarded', 1, true))
    assert(#vim.split(progress, '\n') == 6, 'progress did not retain five log lines')
    vim.fn.writefile({}, release)
    wait(function()
      return finished
    end)
    local final = notices[#notices]
    assert(
      final.message:match('^[^\n]+')
        == (exit_code == 0 and 'Compilation OK' or 'Compilation failed')
    )
    assert(
      final.message:find('diagnostic', 1, true),
      'stderr omitted from compilation notification'
    )
    assert(final.level == (exit_code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR))
    local count = #notices
    vim.wait(150, function()
      return false
    end, 10)
    assert(#notices == count, 'delayed progress overwrote compilation result')
    for _, notice in ipairs(notices) do
      assert(notice.id == final.id, 'compilation updates created separate notifications')
    end
  end
  vim.notify = original_notify
  local result
  project.run({ '/bin/sh', '-c', 'sleep 30 & wait' }, directory, 40, function(value)
    result = value
  end)
  wait(function()
    return result ~= nil
  end)
  assert(result.code ~= 0 and result.stderr:find('timed out'))
  result = nil
  project.run({ '/bin/sh', '-c', 'yes flood' }, directory, 1000, function(value)
    result = value
  end)
  wait(function()
    return result ~= nil
  end)
  assert(result.code ~= 0 and result.stderr:find('1 MiB'))

  -- Root comments select the build/PDF project, not the included source being edited.
  local root_directory = directory .. '/root project'
  vim.fn.mkdir(root_directory .. '/chapters/deep', 'p')
  local root_source = root_directory .. '/Main Root.tex'
  vim.fn.writefile({ '\\documentclass{article}' }, root_source)
  local function root_file(name, lines)
    local path = root_directory .. '/' .. name
    vim.fn.writefile(lines, path)
    return path
  end
  local sibling = root_file('sibling.tex', { '  %  !tEx   RoOt = "Main Root.tex"  ' })
  local nested = root_file('chapters/deep/child.tex', { '% !TEX root = ../parent.tex' })
  root_file('chapters/parent.tex', { "% !TEX root = '../Main Root.tex'" })
  local absolute = root_file('absolute.tex', { '% !TEX root = ' .. root_source })
  assert(vim.uv.fs_symlink(root_directory .. '/chapters/deep', root_directory .. '/linked'))
  local linked = root_file('linked-child.tex', { '% !TEX root = linked/../../Main Root.tex' })
  local boundary_lines = {}
  for index = 1, 19 do
    boundary_lines[index] = '% header'
  end
  boundary_lines[20] = '% !TEX root = Main Root.tex'
  local boundary = root_file('boundary.tex', boundary_lines)
  for _, child in ipairs({ sibling, nested, absolute, linked, boundary }) do
    local described = project.describe(nil, child)
    assert(described.main == vim.uv.fs_realpath(root_source), 'included file remained the build root')
    assert(described.cwd == vim.uv.fs_realpath(root_directory), 'root did not determine working directory')
    assert(described.pdf == root_source:gsub('%.tex$', '.pdf'), 'included file selected its own PDF')
    assert(described.build[#described.build] == described.main, 'default build did not target root')
  end
  for _, directive in ipairs({ 'Main Root.tex', './Main Root.tex', 'self-alias.tex' }) do
    if directive == 'self-alias.tex' then
      assert(vim.uv.fs_symlink(root_source, root_directory .. '/self-alias.tex'))
    end
    vim.fn.writefile({ '% !TEX root = ' .. directive }, root_source)
    for _, child in ipairs({ root_source, nested }) do
      local described = project.describe(nil, child)
      assert(described.main == vim.uv.fs_realpath(root_source), 'self-root did not terminate resolution')
      assert(described.pdf == root_source:gsub('%.tex$', '.pdf'), 'self-root selected the wrong PDF')
    end
  end
  table.insert(boundary_lines, 1, '% header')
  local late = root_file('late.tex', boundary_lines)
  assert(project.describe(nil, late).main == late, 'root comment beyond line 20 was followed')

  local missing = root_file('missing.tex', { '% !TEX root = absent.tex' })
  local wrong_type = root_file('wrong-type.tex', { '% !TEX root = notes.txt' })
  root_file('notes.txt', { 'Not a TeX project.' })
  local cycle = root_file('cycle.tex', { '% !TEX root = chapters/cycle.tex' })
  root_file('chapters/cycle.tex', { '% !TEX root = ../cycle-alias.tex' })
  assert(vim.uv.fs_symlink(cycle, root_directory .. '/cycle-alias.tex'))
  for _, invalid in ipairs({ missing, wrong_type, cycle }) do
    local accepted, error = pcall(project.describe, nil, invalid)
    assert(not accepted and type(error) == 'string', 'invalid root project was accepted: ' .. invalid)
  end

  vim.fn.mkdir(root_directory .. '/output')
  local custom_build = { 'pdflatex', '-output-directory=output', root_source }
  for _, explicit in ipairs({ false, true }) do
    local described = project.describe({
      main = explicit and '../Main Root.tex' or nil,
      cwd = root_directory .. '/output',
      pdf = 'custom.pdf',
      build = custom_build,
    }, explicit and missing or nested)
    assert(described.main == vim.uv.fs_realpath(root_source), 'explicit main did not override root comment')
    assert(described.cwd == vim.uv.fs_realpath(root_directory .. '/output'), 'configured cwd was replaced')
    assert(described.pdf == root_directory .. '/output/custom.pdf', 'configured PDF directory was lost')
    assert(vim.deep_equal(described.build, custom_build), 'configured build command was replaced')
  end

  -- Default latexmk must write the same PDF that describe() selects, even when
  -- the main source is nested below a different compilation directory.
  local default_directory = root_directory .. '/default build'
  vim.fn.mkdir(default_directory .. '/src', 'p')
  local default_main = default_directory .. '/src/main.tex'
  vim.fn.writefile({ '\\documentclass{article}' }, default_main)
  for _, output in ipairs({ false, 'output/custom name.pdf' }) do
    local described = project.describe({
      main = 'src/main.tex', cwd = default_directory, pdf = output or nil,
    })
    local expected_pdf = default_directory .. '/' .. (output or 'src/main.pdf')
    assert(described.main == default_main and described.cwd == default_directory)
    assert(described.pdf == expected_pdf, 'default build selected a different PDF')
    assert(vim.deep_equal(described.build, {
      'latexmk', '-pdf', '-interaction=nonstopmode', '-synctex=1',
      '-outdir=' .. vim.fs.dirname(expected_pdf),
      '-jobname=' .. vim.fs.basename(expected_pdf):gsub('%.pdf$', ''), default_main,
    }), 'default latexmk output disagrees with the selected PDF')
  end

  -- Ownership requires a literal inclusion path, not merely a nearby TeX document.
  local automatic_directory = directory .. '/automatic roots'
  local automatic_cases = {
    {
      name = 'same directory',
      source = 'body.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input body' },
        ['body.tex'] = { 'Included without a root directive.' },
      },
    },
    {
      name = 'dotted input basename',
      source = 'sections/chapter.1.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{sections/chapter.1}' },
        ['sections/chapter.1'] = { 'The .tex candidate takes precedence over this exact name.' },
        ['sections/chapter.1.tex'] = { 'A dotted basename with the .tex extension omitted.' },
      },
    },
    {
      name = 'exact input fallback',
      source = 'body.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{bridge.inc}' },
        ['bridge.inc'] = { '\\input{body}' },
        ['body.tex'] = { 'The exact include name is used when no .tex candidate exists.' },
      },
    },
    {
      name = 'root relative graph',
      source = 'figures/background example.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\include{sections/introduction.tex}' },
        ['sections/introduction.tex'] = { '\\subfile{sections/appendix}' },
        ['sections/appendix.tex'] = { '\\input{sections/introduction}', '\\input{figures/background example}' },
        ['figures/background example.tex'] = { 'A transitive figure.' },
      },
    },
    {
      name = 'configured compilation directory',
      source = 'work/body.tex',
      main = 'main.tex',
      cwd = 'work',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{body}' },
        ['work/body.tex'] = { 'Resolved from the configured compilation directory.' },
      },
    },
    {
      name = 'absolute include',
      source = 'child.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = {
          '\\documentclass{article}',
          '\\input{' .. automatic_directory .. '/absolute include/child.tex}',
        },
        ['child.tex'] = { 'An absolute literal path with spaces.' },
      },
    },
    {
      name = 'nearest owner',
      source = 'nested/body.tex',
      main = 'nested/local.tex',
      files = {
        ['outer.tex'] = { '\\documentclass{article}', '\\input{nested/body}' },
        ['nested/local.tex'] = { '\\documentclass{article}', '\\input{body}' },
        ['nested/body.tex'] = { 'The nearest owner wins.' },
      },
    },
    {
      name = 'standalone source',
      source = 'nested/body.tex',
      main = 'nested/body.tex',
      files = {
        ['outer.tex'] = { '\\documentclass{article}', '\\input{nested/body}' },
        ['nested/body.tex'] = { '\\documentclass{article}' },
      },
    },
    {
      name = 'not included',
      source = 'body.tex',
      main = 'body.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{other}' },
        ['other.tex'] = { 'Not the source.' },
        ['body.tex'] = { 'An unrelated source.' },
        ['archive/old.tex'] = { '\\documentclass{article}', '\\input{../body}' },
      },
    },
    {
      name = 'ignored commands',
      source = 'body.tex',
      main = 'body.tex',
      files = {
        ['main.tex'] = {
          '\\documentclass{article}',
          '% \\input{body}',
          '\\\\input{body}',
          '\\\\include{body}',
          '\\verb|\\input{body}|',
          '\\verb*+\\include{body}+',
          '\\begin{verbatim}',
          '\\input{body}',
          '\\end{verbatim}',
          '\\input{\\target}',
          '\\input{body\\suffix}',
          '\\input{body#1}',
        },
        ['fake-root.tex'] = { '% \\documentclass{article}', '\\input{body}' },
        ['escaped-root.tex'] = { '\\\\documentclass{article}', '\\input{body}' },
        ['body.tex'] = { 'No real inclusion command reaches this source.' },
      },
    },
    {
      name = 'dynamic path',
      source = 'body#1.tex',
      main = 'body#1.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{body#1}' },
        ['body#1.tex'] = { 'A macro parameter is not a literal path, even if a matching file exists.' },
      },
    },
    {
      name = 'escaped percent',
      source = 'body.tex',
      main = 'main.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\% \\input% comment before path', '{body}' },
        ['body.tex'] = { 'An escaped percent is not a comment.' },
      },
    },
    {
      name = 'vcs boundary',
      source = 'nested/body.tex',
      main = 'nested/body.tex',
      files = {
        ['main.tex'] = { '\\documentclass{article}', '\\input{nested/body}' },
        ['nested/.jj/marker'] = {},
        ['nested/body.tex'] = { 'An outer repository must not own this source.' },
      },
    },
  }
  for _, case in ipairs(automatic_cases) do
    local base = automatic_directory .. '/' .. case.name
    vim.fn.mkdir(base .. '/.git', 'p')
    for name, lines in pairs(case.files) do
      vim.fn.mkdir(vim.fs.dirname(base .. '/' .. name), 'p')
      vim.fn.writefile(lines, base .. '/' .. name)
    end
    local options = case.cwd and { cwd = base .. '/' .. case.cwd } or nil
    local described = project.describe(options, base .. '/' .. case.source)
    local expected_main = assert(vim.uv.fs_realpath(base .. '/' .. case.main))
    assert(described.main == expected_main, case.name .. ': incorrect source owner')
    assert(
      described.cwd == vim.uv.fs_realpath(case.cwd and base .. '/' .. case.cwd or vim.fs.dirname(expected_main)),
      case.name .. ': incorrect compilation directory'
    )
    assert(described.pdf == expected_main:gsub('%.tex$', '.pdf'), case.name .. ': incorrect PDF')
    assert(described.build[#described.build] == expected_main, case.name .. ': default compiler targets the wrong source')
  end

  local graph_directory = automatic_directory .. '/changing graph'
  vim.fn.mkdir(graph_directory .. '/.git', 'p')
  local graph_main, graph_other, graph_child = graph_directory .. '/main.tex',
    graph_directory .. '/other.tex', graph_directory .. '/child.tex'
  vim.fn.writefile({ '\\documentclass{article}', '\\input{cycle}', '\\input{child}' }, graph_main)
  vim.fn.writefile({ '\\input{main-alias}' }, graph_directory .. '/cycle.tex')
  vim.fn.writefile({ 'The graph contains a canonical-path cycle.' }, graph_child)
  assert(vim.uv.fs_symlink(graph_main, graph_directory .. '/main-alias.tex'))
  assert(project.describe(nil, graph_child).main == graph_main, 'symlink cycle or duplicate owner broke discovery')
  vim.fn.writefile({ '\\documentclass{article}' }, graph_main)
  assert(project.describe(nil, graph_child).main == graph_child, 'removed inclusion remained cached')
  vim.fn.writefile({ '\\documentclass{article}', '\\input{child}' }, graph_other)
  assert(project.describe(nil, graph_child).main == graph_other, 'new inclusion was not discovered')
  vim.fn.writefile({ '\\documentclass{article}', '\\input{child}' }, graph_main)
  local accepted, ambiguity = pcall(project.describe, nil, graph_child)
  assert(not accepted and type(ambiguity) == 'string', 'ambiguous owners silently selected a root')
  assert(
    ambiguity:find(graph_main, 1, true) and ambiguity:find(graph_other, 1, true)
      and ambiguity:find(':PdfTermMain', 1, true) and ambiguity:find('project.main', 1, true),
    'ambiguous owners did not identify candidates and explicit selection'
  )
  assert(project.describe({ main = graph_other }, graph_child).main == graph_other, 'explicit main did not resolve ambiguity')
  vim.fn.writefile({ '% !TEX root = main.tex' }, graph_child)
  assert(project.describe(nil, graph_child).main == graph_main, 'root directive did not override automatic ambiguity')
  assert(project.describe({ main = graph_other }, graph_child).main == graph_other, 'explicit main lost to a root directive')
  vim.fn.writefile({ '% !TEX root = child.tex' }, graph_child)
  assert(project.describe(nil, graph_child).main == graph_child, 'self-root lost to automatic ownership')

  vim.fn.writefile(
    { 'forward_socket="forward.sock"', '[editor]', 'transport="socket"', 'path="editor.sock"' },
    directory .. '/pdfterm/config.toml'
  )
  local binary = assert(vim.env.PDFTERM_EXECUTABLE, 'set PDFTERM_EXECUTABLE to a built viewer')
  local wrapper = directory .. '/selected-viewer'
  vim.fn.writefile({
    '#!/bin/sh',
    'echo selected >> ' .. vim.fn.shellescape(directory .. '/bootstrap.log'),
    'if test -f ' .. vim.fn.shellescape(directory .. '/hold-resolution') .. '; then',
    '  touch ' .. vim.fn.shellescape(directory .. '/resolution-started'),
    '  while test -f '
      .. vim.fn.shellescape(directory .. '/hold-resolution')
      .. '; do sleep .01; done',
    'fi',
    'exec ' .. vim.fn.shellescape(binary) .. ' "$@"',
  }, wrapper)
  assert(vim.uv.fs_chmod(wrapper, 448))
  local source = directory .. '/navigation.tex'
  vim.fn.writefile(vim.fn.readfile(root .. '/tests/fixtures/navigation.tex'), source)
  vim.fn.mkdir(directory .. '/artifacts')
  local pdf = directory .. '/artifacts/navigation.pdf'
  local adapter = require('pdfterm')
  adapter.setup({
    executable = wrapper,
    session = 'adapter',
    attach_only = false,
    focus_on_inverse = true,
    focus_on_forward = true,
    compile = true,
    project = {
      main = 'navigation.tex',
      pdf = 'artifacts/navigation.pdf',
      cwd = directory,
      build = {
        '/bin/sh',
        '-c',
        'if test -f fail-build; then echo "deliberate build failure" >&2; exit 1; fi; '
          .. 'sleep .05; exec pdflatex -interaction=nonstopmode -halt-on-error -synctex=1 -output-directory=artifacts navigation.tex',
      },
    },
  })
  wait(function()
    return vim.fn.filereadable(directory .. '/bootstrap.log') == 1
  end)
  assert(
    vim.fn.readfile(directory .. '/bootstrap.log')[1] == 'selected',
    'bootstrap executable ignored'
  )
  assert(
    vim.fn.exists(':PdfTermForward') == 2
      and vim.fn.exists(':PdfTermForwardSplit') == 2
  )
  assert(vim.fn.exists(':PdfTermBuild') == 2)
  for _, mapping in ipairs(vim.api.nvim_get_keymap('n')) do
    assert(not (mapping.desc or ''):match('^pdfterm'), 'default mappings are not opt-in')
  end
  local config = vim.json.decode(
    vim
      .system({ binary, '--session', 'adapter', '--print-config' }, { text = true })
      :wait(10000).stdout
  )
  assert(config.forward_socket:match('/adapter%-forward.sock$'))
  assert(
    not vim.uv.fs_lstat(config.editor.path),
    'setup opened an inverse listener before first use'
  )
  local requests, focus_requests, viewer_reply_token = {}, {}, nil
  local function receive()
    server = assert(vim.uv.new_pipe(false))
    assert(server:bind(config.forward_socket))
    server:listen(16, function(error)
      assert(not error, error)
      local client = assert(vim.uv.new_pipe(false))
      server:accept(client)
      local chunks = {}
      client:read_start(function(read_error, chunk)
        assert(not read_error, read_error)
        if chunk then
          chunks[#chunks + 1] = chunk
        else
          local request = vim.json.decode(table.concat(chunks))
          if request.type == 'focus' then
            focus_requests[#focus_requests + 1] = request
          else
            requests[#requests + 1] = request
          end
          client:write(vim.json.encode({
            ok = true,
            error = vim.NIL,
            viewer_token = viewer_reply_token,
          }), function()
            client:shutdown(function()
              client:close()
            end)
          end)
        end
      end)
    end)
  end
  receive()
  vim.cmd.edit(vim.fn.fnameescape(source))
  vim.api.nvim_win_set_cursor(0, { 4, 0 })
  adapter.forward()
  vim.api.nvim_win_set_cursor(0, { 6, 0 })
  adapter.forward()
  local ticks = 0
  local timer = assert(vim.uv.new_timer())
  timer:start(1, 5, function()
    ticks = ticks + 1
  end)
  wait(function()
    return #requests == 1
  end)
  timer:stop()
  timer:close()
  assert(ticks > 2, 'build/resolution blocked Neovim events')
  local resolved = vim
    .system({ binary, pdf, '--synctex-view', source, '--line', '6', '--column', '1' }, { text = true })
    :wait(10000)
  assert(resolved.code == 0, resolved.stderr)
  local expected = vim.json.decode(resolved.stdout)
  assert(requests[1].v == expected.v and requests[1].h == expected.h, 'latest forward intent lost')
  assert(#vim.fn.readfile(directory .. '/bootstrap.log') == 2, 'obsolete build reached resolution')
  -- Opening an existing PDF must not invoke TeX/SyncTeX, even with compile enabled.
  assert(vim.uv.fs_unlink(directory .. '/artifacts/navigation.synctex.gz'))
  local linked_pdf = directory .. '/linked PDF.PDF'
  assert(vim.uv.fs_symlink(pdf, linked_pdf))
  adapter.open(linked_pdf)
  wait(function()
    return #requests == 2
  end)
  assert(requests[2].pdf == vim.uv.fs_realpath(pdf) and requests[2].page == 1)
  assert(
    vim.deep_equal(requests[2].revision, expected.revision),
    'PDF revision differs from native metadata'
  )
  assert(
    not vim.uv.fs_stat(directory .. '/artifacts/navigation.synctex.gz'),
    'opening PDF unexpectedly rebuilt TeX'
  )
  -- A missing SyncTeX sidecar must not prevent opening the existing PDF.
  adapter.toggle_compile()
  local navigation_notices = {}
  vim.notify = function(message, level)
    assert(not vim.in_fast_event(), 'navigation warning emitted from process callback')
    navigation_notices[#navigation_notices + 1] = { message = message, level = level }
  end
  adapter.forward()
  wait(function()
    return #requests == 3
  end)
  assert(requests[3].pdf == vim.uv.fs_realpath(pdf) and requests[3].page == 1)
  wait(function()
    return #navigation_notices >= 2
  end)
  assert(
    #navigation_notices == 2
      and navigation_notices[1].level == vim.log.levels.WARN
      and navigation_notices[2].message:find('viewer focus unavailable', 1, true)
  )
  -- Superseding an in-flight resolution must suppress its warning and fallback.
  navigation_notices = {}
  vim.fn.writefile({}, directory .. '/hold-resolution')
  adapter.forward()
  wait(function()
    return vim.fn.filereadable(directory .. '/resolution-started') == 1
  end)
  adapter.open(linked_pdf)
  wait(function()
    return #requests == 4
  end)
  vim.wait(150, function()
    return false
  end, 10)
  assert(
    #requests == 4 and #navigation_notices == 0,
    'superseded resolution still opened or warned'
  )
  assert(vim.uv.fs_unlink(directory .. '/hold-resolution'))

  -- Build failure is not a SyncTeX failure: do not open an old PDF.
  adapter.toggle_compile()
  navigation_notices = {}
  vim.fn.writefile({}, directory .. '/fail-build')
  adapter.forward()
  wait(function()
    for _, notice in ipairs(navigation_notices) do
      if notice.level == vim.log.levels.ERROR then
        return true
      end
    end
    return false
  end)
  vim.wait(150, function()
    return false
  end, 10)
  assert(#requests == 4, 'failed build opened stale output')
  vim.notify = original_notify
  local function inverse_jump()
    local inverse = assert(vim.uv.new_pipe(false))
    inverse:connect(config.editor.path, function(error)
      assert(not error, error)
      inverse:write(vim.json.encode({ file = source, line = 4, byte_column = 6 }), function()
        inverse:shutdown(function()
          inverse:close()
        end)
      end)
    end)
  end
  inverse_jump()
  wait(function()
    return vim.api.nvim_win_get_cursor(0)[1] == 4
  end)
  assert(vim.api.nvim_win_get_cursor(0)[2] == 6)
  -- The terminal selected before a slow resolver owns launch/inverse focus.
  vim.env.SSH_CONNECTION = nil
  local foreground, focused = 'A', nil
  local focus_notices = {}
  vim.notify = function(message)
    focus_notices[#focus_notices + 1] = message
  end
  terminal.capture_source = function(callback)
    local captured = foreground
    vim.defer_fn(function()
      callback(nil, { kind = 'ghostty', id = captured })
    end, 10)
  end
  terminal.focus = function(handle, callback)
    focused = handle.id
    callback({ code = 0 })
  end
  adapter.toggle_compile()
  vim.uv.fs_unlink(directory .. '/resolution-started')
  vim.fn.writefile({}, directory .. '/hold-resolution')
  adapter.forward()
  wait(function()
    return vim.fn.filereadable(directory .. '/resolution-started') == 1
  end)
  foreground = 'B'
  vim.uv.fs_unlink(directory .. '/hold-resolution')
  wait(function()
    return #requests == 5
  end)
  wait(function()
    return focus_notices[#focus_notices]
      and focus_notices[#focus_notices]:find('viewer focus unavailable', 1, true)
  end)
  assert(focused == nil, 'external viewer was mistaken for a plugin-owned split')
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'A', 'slow navigation retargeted inverse focus to a later terminal')
  -- Manual pairing does not expose a runnable command until asynchronous capture completes.
  local manual_capture
  terminal.capture_source = function(callback)
    manual_capture = callback
  end
  local command_notices = #focus_notices
  adapter.viewer_command(pdf)
  assert(manual_capture and #focus_notices == command_notices)
  assert(adapter._source_terminal == nil, 'old source survived a new manual pairing')
  manual_capture(nil, { kind = 'ghostty', id = 'manual-source' })
  wait(function()
    return adapter._source_terminal and adapter._source_terminal.id == 'manual-source'
      and #focus_notices > command_notices
  end)
  assert(
    focus_notices[#focus_notices]:find(root .. '/scripts/pdfterm-viewer', 1, true),
    'focused manual command omitted the terminal-aware launcher'
  )
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'manual-source', 'manual viewer command did not retain invocation terminal')

  -- Explicit source handles win; socket attachment still works without capture.
  terminal.capture_source = function()
    error('supplied source must not be recaptured')
  end
  adapter.forward_search(pdf, vim.json.encode(requests[1]), { kind = 'ghostty', id = 'explicit' })
  wait(function()
    return #requests == 6
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'explicit')
  terminal.capture_source = function(callback)
    callback('terminal discovery unavailable')
  end
  adapter.open(pdf)
  wait(function()
    return #requests == 7
  end)

  -- A late capture cannot resurrect a superseded navigation.
  local captures = {}
  terminal.capture_source = function(callback)
    captures[#captures + 1] = callback
  end
  adapter.open(pdf)
  adapter.open(pdf)
  captures[2](nil, { kind = 'ghostty', id = 'new' })
  captures[1](nil, { kind = 'ghostty', id = 'old' })
  wait(function()
    return #requests == 8
  end)
  vim.wait(50, function()
    return false
  end, 5)
  assert(#requests == 8, 'superseded terminal capture delivered navigation')
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'new', 'superseded capture changed inverse focus')
  -- Ordinary forward only attaches; explicit split launches and then delivers.
  server:close()
  local launches, notices, closed_splits = 0, {}, {}
  terminal.launch_split = function(_, _, _, callback, _, token)
    launches = launches + 1
    viewer_reply_token = token
    receive()
    vim.schedule(function()
      callback({ code = 0 }, { kind = 'ghostty', id = 'viewer' })
    end)
    return { wait = function() end }
  end
  terminal.capture_source = function(callback)
    callback(nil, { kind = 'ghostty', id = 'source' })
  end
  terminal.close = function(split)
    assert(split.id == 'viewer' or split.id == 'viewer2' or split.id == 'viewer3')
    closed_splits[split.id] = (closed_splits[split.id] or 0) + 1
  end
  vim.notify = function(message, level)
    notices[#notices + 1] = { message = message, level = level }
  end
  vim.cmd('PdfTermForward')
  wait(function()
    return #notices >= 2
  end)
  assert(launches == 0 and #requests == 8, 'ordinary forward launched a viewer')
  assert(notices[#notices].message:find('PdfTermForwardSplit', 1, true))
  focused = nil
  vim.cmd('PdfTermForwardSplit')
  wait(function()
    return #requests == 9 and focused == 'viewer'
  end)
  assert(
    launches == 1 and requests[9].page == 1,
    'explicit split did not launch and focus its viewer after forward'
  )
  focused = nil
  adapter.forward_search(pdf, vim.json.encode(requests[1]), { kind = 'ghostty', id = 'source' })
  wait(function()
    return #requests == 10 and focused == 'viewer'
  end)
  focused = nil
  adapter.open(pdf)
  wait(function()
    return #requests == 11
  end)
  assert(focused == nil, 'opening an existing PDF unexpectedly changed terminal focus')
  viewer_reply_token = string.rep('a', 32)
  adapter.forward_search(pdf, vim.json.encode(requests[1]), { kind = 'ghostty', id = 'source' })
  wait(function()
    return #requests == 12 and #focus_requests == 1
  end)
  assert(
    focus_requests[1].viewer_token == viewer_reply_token,
    'manual viewer focus did not use its acknowledged token'
  )
  server:close()
  viewer_reply_token = nil
  receive()
  local before = #notices
  adapter.forward_search(pdf, vim.json.encode(requests[1]), { kind = 'ghostty', id = 'source' })
  wait(function()
    return #requests == 13 and #notices > before
  end)
  assert(focused == nil and notices[#notices].message:find('viewer focus unavailable', 1, true))
  -- Another viewer can win the socket race after a split is launched.
  server:close()
  terminal.launch_split = function(_, _, _, callback)
    launches = launches + 1
    viewer_reply_token = 'unrelated-viewer'
    receive()
    vim.schedule(function()
      callback({ code = 0 }, { kind = 'ghostty', id = 'viewer2' })
    end)
    return { wait = function() end }
  end
  before = #notices
  vim.cmd('PdfTermForwardSplit')
  wait(function()
    return #requests == 14
      and #notices > before
      and notices[#notices].message:find('viewer focus unavailable', 1, true)
  end)
  assert(focused == nil and launches == 2, 'racing viewer stole focus from the actual responder')
  -- Failure after splitting must report failure even when failed rollback
  -- transfers a live provisional handle for editor-exit cleanup.
  server:close()
  terminal.launch_split = function(_, _, _, callback)
    launches = launches + 1
    vim.schedule(function()
      callback({ code = 1, stderr = 'source refocus and rollback failed', unclosed = true },
        { kind = 'ghostty', id = 'viewer3' })
    end)
    return { wait = function() end }
  end
  before = #notices
  local delivered = #requests
  vim.cmd('PdfTermForwardSplit')
  wait(function()
    for i = before + 1, #notices do
      if notices[i].level == vim.log.levels.ERROR
        and notices[i].message:find('source refocus and rollback failed', 1, true)
      then
        return true
      end
    end
    return false
  end)
  assert(#requests == delivered, 'failed split was reported as usable')
  receive()

  -- Reinitialize through the plugin's cleanup path: setup itself is intentionally idempotent.
  -- A relative compiler argument must resolve in the inferred root cwd, not the editor/source cwd.
  local included_directory = directory .. '/included project'
  vim.fn.mkdir(included_directory .. '/chapters/deep', 'p')
  vim.fn.mkdir(included_directory .. '/output/chapters', 'p')
  vim.fn.mkdir(included_directory .. '/figures')
  local included_main = included_directory .. '/main.tex'
  vim.fn.writefile({
    '% Included sources below exercise independent root-selection paths.',
    '\\documentclass{article}',
    '\\pagestyle{empty}',
    '\\begin{document}',
    'Root page is not an included source target.',
    '\\newpage',
    '\\input{same}',
    '\\newpage',
    '\\input{chapters/deep/nested}',
    '\\newpage',
    '\\input{automatic}',
    '\\include{chapters/chain}',
    '\\end{document}',
  }, included_main)
  vim.fn.writefile({ '% !TEX root = ../main.tex' }, included_directory .. '/chapters/root.tex')
  vim.fn.writefile({
    '\\input{chapters/deep/automatic.1}',
    '\\newpage',
    '\\input{figures/transitive}',
  }, included_directory .. '/chapters/chain.tex')
  local included_cases = {
    { file = 'automatic.tex', page = 4, word = 'saffron' },
    { file = 'chapters/deep/automatic.1.tex', page = 5, word = 'cobalt' },
    { file = 'figures/transitive.tex', page = 6, word = 'azimuth' },
    { file = 'same.tex', directive = 'main.tex', page = 2, word = 'zephyr' },
    { file = 'chapters/deep/nested.tex', directive = '../root.tex', page = 3, word = 'quartz' },
  }
  for _, case in ipairs(included_cases) do
    case.path = included_directory .. '/' .. case.file
    case.text = 'The caf' .. string.char(195, 169) .. ' contains the distinct target ' .. case.word .. '.'
    vim.fn.writefile({
      case.directive and '% !TEX root = ' .. case.directive or '% No root directive.',
      '',
      'A different paragraph must not become the cursor target.',
      '',
      case.text,
    }, case.path)
  end
  vim.notify = original_notify
  for _, output_folder in ipairs({ '.', 'output' }) do
    vim.api.nvim_exec_autocmds('VimLeavePre', { group = 'pdfterm' })
    if output_folder == '.' then
      assert(closed_splits.viewer3 == 1, 'failed rollback lost its owned viewer')
    end
    wait(function()
      return not vim.uv.fs_lstat(config.editor.path)
    end)
    package.loaded['pdfterm'] = nil
    adapter = require('pdfterm')
    local included_pdf = included_directory
      .. (output_folder == '.' and '/main.pdf' or '/output/main.pdf')
    assert(not vim.uv.fs_stat(included_pdf), 'integration reused an already-built output PDF')
    adapter.setup({
      executable = binary,
      session = 'adapter',
      attach_only = true,
      focus_on_inverse = false,
      focus_on_forward = false,
      compile = true,
      project = {
        pdf = output_folder == 'output' and 'output/main.pdf' or nil,
        build = {
          'pdflatex',
          '-interaction=nonstopmode',
          '-halt-on-error',
          '-synctex=1',
          '-output-directory=' .. output_folder,
          'main.tex',
        },
      },
    })
    for _, case in ipairs(included_cases) do
      vim.cmd.edit(vim.fn.fnameescape(case.path))
      local byte_column = assert(case.text:find(case.word, 1, true)) + #case.word - 2
      vim.api.nvim_win_set_cursor(0, { 5, byte_column })
      local previous = #requests
      adapter.forward()
      wait(function()
        return #requests == previous + 1
      end)
      local request = requests[#requests]
      assert(request.pdf == vim.uv.fs_realpath(included_pdf), 'forward opened an included-file PDF')
      assert(request.page == case.page, 'forward lost the included source page')
      assert(
        request.word and request.word.words[request.word.selected + 1] == case.word,
        'forward lost the original included text or Unicode cursor column'
      )
    end
  end
  -- Attaching never requires capture, but requested inverse focus retains a
  -- manually paired terminal when discovery is temporarily unavailable.
  vim.api.nvim_exec_autocmds('VimLeavePre', { group = 'pdfterm' })
  wait(function()
    return not vim.uv.fs_lstat(config.editor.path)
  end)
  package.loaded['pdfterm'] = nil
  adapter = require('pdfterm')
  adapter.setup({
    executable = binary,
    session = 'adapter',
    attach_only = true,
    focus_on_inverse = true,
    focus_on_forward = false,
    compile = false,
    project = { main = source, pdf = pdf, cwd = directory },
  })
  local attachment_notices = {}
  vim.notify = function(message)
    attachment_notices[#attachment_notices + 1] = tostring(message)
  end
  terminal.launch_split = function()
    error('attach_only must not launch a terminal')
  end
  terminal.capture_source = function(callback)
    callback('terminal discovery unavailable')
  end
  vim.cmd.edit(vim.fn.fnameescape(source))
  vim.api.nvim_win_set_cursor(0, { 1, 0 })
  local previous = #requests
  vim.cmd('PdfTermForward')
  wait(function()
    return #requests == previous + 1
      and attachment_notices[#attachment_notices]
      and attachment_notices[#attachment_notices]:find('terminal discovery unavailable', 1, true)
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return vim.deep_equal(vim.api.nvim_win_get_cursor(0), { 4, 6 })
      and attachment_notices[#attachment_notices]:find('source terminal unavailable', 1, true)
  end)
  assert(focused == nil, 'unavailable capture invented a source terminal')

  terminal.capture_source = function(callback)
    callback(nil, { kind = 'ghostty', id = 'attached-manual-source' })
  end
  vim.cmd('PdfTermViewerCommand ' .. vim.fn.fnameescape(pdf))
  wait(function()
    return attachment_notices[#attachment_notices]:find('--session', 1, true)
  end)
  terminal.capture_source = function()
    error('terminal discovery temporarily unavailable')
  end
  previous = #requests
  vim.cmd('PdfTermForward')
  wait(function()
    return #requests == previous + 1
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'attached-manual-source', 'attachment discarded the manually paired source')

  terminal.capture_source = function(callback)
    callback(nil, { kind = 'ghostty', id = 'attached-current-source' })
  end
  previous = #requests
  vim.cmd('PdfTermForward')
  wait(function()
    return #requests == previous + 1
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'attached-current-source', 'attachment skipped requested source capture')

  terminal.capture_source = function()
    error('explicit attachment source must not be recaptured')
  end
  previous = #requests
  adapter.forward_search(pdf, vim.json.encode(requests[1]),
    { kind = 'ghostty', id = 'attached-explicit-source' })
  wait(function()
    return #requests == previous + 1
  end)
  focused = nil
  inverse_jump()
  wait(function()
    return focused ~= nil
  end)
  assert(focused == 'attached-explicit-source', 'attachment ignored its explicit source')
  vim.notify = original_notify
  print(
    'adapter regressions passed: builds, timeouts, bootstrap, sessions, root projects, included forward, forward focus, inverse focus, split ownership; event ticks='
      .. ticks
  )
end, debug.traceback)
if server and not server:is_closing() then
  server:close()
end
-- VimLeavePre cleans the owned inverse socket and active process groups first.
vim.api.nvim_create_autocmd('VimLeavePre', {
  once = true,
  callback = function()
    vim.fn.delete(directory, 'rf')
  end,
})
if not ok then
  io.stderr:write(failure .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
