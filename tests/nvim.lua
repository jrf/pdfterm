-- Run: PDFTERM_EXECUTABLE="$PWD/target/debug/pdfterm" nvim --headless -u NONE -l tests/nvim.lua
local root = vim.fn.getcwd()
vim.opt.runtimepath:prepend(root)
local copied_commands = {}
local function capture_clipboard(lines)
  copied_commands[#copied_commands + 1] = table.concat(lines, '\n')
end
vim.g.clipboard = {
  name = 'test clipboard',
  copy = {
    ['+'] = capture_clipboard,
    ['*'] = capture_clipboard,
  },
  paste = {
    ['+'] = function() return {}, 'v' end,
    ['*'] = function() return {}, 'v' end,
  },
}
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
  vim.fn.mkdir(root_directory .. '/unreadable.tex')
  local unreadable = root_file('unreadable-root.tex', { '% !TEX root = unreadable.tex' })
  local cycle = root_file('cycle.tex', { '% !TEX root = chapters/cycle.tex' })
  root_file('chapters/cycle.tex', { '% !TEX root = ../cycle-alias.tex' })
  assert(vim.uv.fs_symlink(cycle, root_directory .. '/cycle-alias.tex'))
  for _, invalid in ipairs({ missing, wrong_type, unreadable, cycle }) do
    for _, current in ipairs({ false, table.concat(vim.fn.readfile(invalid), '\n') }) do
      local accepted, error = pcall(project.describe, nil, invalid, current or nil)
      assert(not accepted and type(error) == 'string', 'invalid root project was accepted: ' .. invalid)
    end
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
  local unsaved_sub = graph_directory .. '/sub'
  vim.fn.mkdir(unsaved_sub)
  local unsaved_standalone = unsaved_sub .. '/unsaved.tex'
  vim.fn.writefile({ '\\documentclass{article}', '\\input{sub/unsaved}' }, graph_main)
  assert(
    project.describe(nil, unsaved_standalone, '\\documentclass{article}\n\\begin{document}\nstandalone\n\\end{document}\n').main
      == unsaved_standalone,
    'unsaved standalone document was misrouted to parent root'
  )
  assert(
    project.describe(nil, unsaved_standalone, 'Included unsaved section.\n').main == graph_main,
    'unsaved child fragment did not route to parent root'
  )

  vim.fn.writefile(
    { 'forward_socket="forward.sock"', '[editor]', 'transport="socket"', 'path="editor.sock"' },
    directory .. '/pdfterm/config.toml'
  )
  local binary = assert(
    vim.env.PDFTERM_EXECUTABLE
      or (vim.fn.executable(root .. '/target/debug/pdfterm') == 1 and (root .. '/target/debug/pdfterm'))
      or (vim.fn.executable(root .. '/target/release/pdfterm') == 1 and (root .. '/target/release/pdfterm'))
      or (vim.fn.exepath('pdfterm') ~= '' and vim.fn.exepath('pdfterm')),
    'set PDFTERM_EXECUTABLE to a built viewer'
  )
  local wrapper = directory .. '/selected-viewer'
  vim.fn.writefile({
    '#!/bin/sh',
    'echo selected >> ' .. vim.fn.shellescape(directory .. '/bootstrap.log'),
    'for argument do',
    '  if test "$argument" = --synctex-view && test -f ' .. vim.fn.shellescape(directory .. '/hold-resolution') .. '; then',
    '    touch ' .. vim.fn.shellescape(directory .. '/resolution-started'),
    '    while test -f '
      .. vim.fn.shellescape(directory .. '/hold-resolution')
      .. '; do sleep .01; done',
    '  fi',
    'done',
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
  local requests, focus_requests, quit_requests, viewer_reply_token = {}, {}, {}, nil
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
          elseif request.type == 'quit' then
            quit_requests[#quit_requests + 1] = request
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
  -- External compilation has no editor capture, but best-guess forwarding can still carry a saved-source hint.
  local external_build = vim.system({
    'pdflatex', '-interaction=nonstopmode', '-halt-on-error', '-synctex=1',
    '-output-directory=artifacts', 'navigation.tex',
  }, { cwd = directory, text = true }):wait(10000)
  assert(external_build.code == 0, external_build.stdout .. external_build.stderr)
  adapter.toggle_compile()
  local target_line = vim.api.nvim_buf_get_lines(0, 5, 6, false)[1]
  local target_byte = assert(target_line:find('zephyr', 1, true)) - 1
  vim.api.nvim_win_set_cursor(0, { 6, target_byte })
  local forward_notices, forward_notify = {}, vim.notify
  vim.notify = function(message, level)
    forward_notices[#forward_notices + 1] = { message = message, level = level }
  end
  local get_lines, full_copies = vim.api.nvim_buf_get_lines, 0
  vim.api.nvim_buf_get_lines = function(buffer, first, last, strict)
    if first == 0 and last == -1 then full_copies = full_copies + 1 end
    return get_lines(buffer, first, last, strict)
  end
  adapter.forward(false)
  wait(function() return #requests == 1 end)
  vim.api.nvim_buf_get_lines = get_lines
  assert(full_copies == 0, 'unmodified TeX forward copied the full source buffer')
  vim.notify = forward_notify
  for _, notice in ipairs(forward_notices) do
    assert(notice.level ~= vim.log.levels.WARN and notice.level ~= vim.log.levels.ERROR,
      'routine saved-source forward diagnostic raised a warning: ' .. vim.inspect(notice))
  end
  local uncaptured = table.remove(requests)
  assert(uncaptured.best_guess == true and type(uncaptured.word) == 'table'
    and uncaptured.word.words[uncaptured.word.selected + 1] == 'zephyr',
    'absent compiler capture suppressed the saved-source best-guess hint')
  assert(type(uncaptured.warning) == 'string' and uncaptured.warning ~= '',
    'uncaptured saved-source hint lost its warning')
  local virtualedit = vim.o.virtualedit
  vim.o.virtualedit = 'onemore'
  vim.api.nvim_win_set_cursor(0, { 6, #target_line })
  adapter.forward(false)
  wait(function() return #requests == 1 end)
  assert(table.remove(requests).word == vim.NIL, 'line boundary acquired a saved-source hint')
  vim.o.virtualedit = virtualedit
  vim.api.nvim_win_set_cursor(0, { 6, target_byte })
  local source_stat = assert(vim.uv.fs_stat(source))
  assert(vim.uv.fs_utime(source, source_stat.atime.sec, assert(vim.uv.fs_stat(pdf)).mtime.sec + 60))
  adapter.forward(false)
  wait(function() return #requests == 1 end)
  assert(table.remove(requests).word == vim.NIL, 'known-newer source acquired a saved-source hint')
  assert(vim.uv.fs_utime(source, source_stat.atime.sec,
    source_stat.mtime.sec + source_stat.mtime.nsec / 1e9))
  adapter.forward(false)
  vim.cmd.file(vim.fn.fnameescape(directory .. '/renamed-navigation.tex'))
  wait(function() return #requests == 1 end)
  assert(table.remove(requests).word == vim.NIL, 'renamed source acquired a hint for its old identity')
  vim.cmd.file(vim.fn.fnameescape(source))
  adapter.toggle_compile()
  local saved_lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  for _, edit in ipairs({ 'line', 'unicode', 'post-write' }) do
    local edited, notify = false, vim.notify
    if edit == 'post-write' then
      vim.bo.modified = true
      vim.api.nvim_create_autocmd('BufWritePost', {
        buffer = vim.api.nvim_get_current_buf(),
        once = true,
        callback = function()
          edited = true
          vim.api.nvim_buf_set_lines(0, 0, 0, false, { '% Unsaved after write 1.', '% Unsaved after write 2.' })
          vim.api.nvim_buf_set_lines(0, 7, 8, false, { string.rep('雪', 20) .. target_line })
        end,
      })
    else
      vim.notify = function(message, ...)
        if not edited and message:match('^Compiling') then
          edited = true
          if edit == 'line' then
            vim.api.nvim_buf_set_lines(0, 0, 0, false, { '% Unsaved while compiling.' })
          else
            vim.api.nvim_buf_set_lines(0, 5, 6, false, { string.rep('雪', 20) .. target_line })
          end
        end
        notify(message, ...)
      end
    end
    vim.api.nvim_win_set_cursor(0, { 6, target_byte })
    adapter.forward(false)
    wait(function() return #requests == 1 end)
    vim.notify = notify
    local request = table.remove(requests)
    assert(edited and (edit == 'post-write' or vim.bo.modified), 'unsaved edit was not exercised: ' .. edit)
    assert(request.h == uncaptured.h and request.v == uncaptured.v,
      'unsaved edit moved the saved-source destination: ' .. edit)
    assert(type(request.word) == 'table'
      and request.word.words[request.word.selected + 1] == 'zephyr',
      'unsaved edit selected a different saved-source word: ' .. edit)
    vim.api.nvim_buf_set_lines(0, 0, -1, false, saved_lines)
    vim.cmd('write!')
  end
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
  -- Opening an existing PDF must not invoke TeX/SyncTeX, even with compile enabled.
  local sidecar = directory .. '/artifacts/navigation.synctex.gz'
  local preserved_sidecar = sidecar .. '.saved'
  assert(vim.uv.fs_rename(sidecar, preserved_sidecar))
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
  -- Unavailable source geometry warns without sending unrelated page-one navigation when launch is disabled.
  adapter.toggle_compile()
  local navigation_notices = {}
  vim.notify = function(message, level)
    assert(not vim.in_fast_event(), 'navigation warning emitted from process callback')
    navigation_notices[#navigation_notices + 1] = { message = message, level = level }
  end
  adapter.forward(false)
  wait(function()
    return #navigation_notices > 0
  end)
  assert(#requests == 2, 'missing source geometry changed the viewer position')
  for _, notice in ipairs(navigation_notices) do
    assert(notice.level == vim.log.levels.WARN, 'unavailable source geometry did not warn')
  end
  -- Default forward / forward_split still opens the PDF at page 1 when source geometry is unavailable.
  navigation_notices = {}
  adapter.forward()
  wait(function()
    return #requests == 3 and #navigation_notices > 0
  end)
  local split_fallback = table.remove(requests)
  assert(split_fallback.page == 1 and split_fallback.best_guess == true, 'forward did not open PDF at page 1')
  for _, notice in ipairs(navigation_notices) do
    assert(notice.level == vim.log.levels.WARN, 'forward unavailable source geometry did not warn')
  end
  -- A usable source line resumes navigation after the sidecar becomes available.
  assert(vim.uv.fs_rename(preserved_sidecar, sidecar))
  adapter.forward()
  wait(function()
    return #requests == 3
  end)
  assert(requests[3].page == expected.page, 'restored source geometry lost the requested line')
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
  local exact_source = directory .. '/exact.tex'
  local backup_source = directory .. '/exact.tex.bak'
  vim.fn.writefile({ 'line one', 'line two' }, exact_source)
  vim.fn.writefile({ 'backup one', 'backup two' }, backup_source)
  local backup_buf = vim.fn.bufadd(backup_source)
  vim.fn.bufload(backup_buf)
  local exact_inverse = assert(vim.uv.new_pipe(false))
  exact_inverse:connect(config.editor.path, function(error)
    assert(not error, error)
    exact_inverse:write(vim.json.encode({ file = exact_source, line = 2, byte_column = 5 }), function()
      exact_inverse:shutdown(function()
        exact_inverse:close()
      end)
    end)
  end)
  wait(function()
    return vim.api.nvim_buf_get_name(0) == exact_source and vim.api.nvim_win_get_cursor(0)[1] == 2
  end)
  assert(vim.api.nvim_win_get_cursor(0)[2] == 5, 'inverse search opened a partial-match backup buffer')
  vim.cmd.edit(vim.fn.fnameescape(source))
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
  local copy_count = #copied_commands
  adapter.viewer_command(pdf)
  assert(manual_capture and #focus_notices == command_notices)
  assert(adapter._source_terminal == nil, 'old source survived a new manual pairing')
  assert(#copied_commands == copy_count, 'manual command copied before source capture')
  manual_capture(nil, { kind = 'ghostty', id = 'manual-source' })
  wait(function()
    return adapter._source_terminal and adapter._source_terminal.id == 'manual-source'
      and #focus_notices > command_notices
  end)
  assert(
    focus_notices[#focus_notices]:find(root .. '/scripts/pdfterm-viewer', 1, true),
    'focused manual command omitted the terminal-aware launcher'
  )
  assert(
    #copied_commands == copy_count + 1
      and copied_commands[#copied_commands] == focus_notices[#focus_notices],
    'manual command was not copied after source capture'
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
  local launches, notices, closed_splits, viewer_token = 0, {}, {}, nil
  terminal.launch_split = function(_, _, _, callback, _, token)
    launches = launches + 1
    viewer_token = token
    viewer_reply_token = token
    receive()
    vim.schedule(function()
      callback({ code = 0 }, { kind = 'ssh', id = 'viewer' })
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
  adapter.forward(false)
  wait(function()
    return #notices >= 1 and notices[#notices].message:find('PdfTermForwardSplit', 1, true) ~= nil
  end)
  assert(launches == 0 and #requests == 8, 'attach-only forward launched a viewer')
  local attach_errors = vim.tbl_filter(function(notice)
    return notice.level == vim.log.levels.ERROR
  end, notices)
  assert(#attach_errors == 1 and attach_errors[1].message:find('PdfTermForwardSplit', 1, true),
    'attach-only forward did not report exactly one viewer-unavailable error')
  focused = nil
  vim.cmd('PdfTermForward')
  wait(function()
    return #requests == 9 and focused == 'viewer'
  end)
  assert(
    launches == 1 and requests[9].page == 1,
    'default forward did not launch and focus its viewer'
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
  viewer_reply_token = viewer_token
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
      assert(closed_splits.viewer == 1, 'ready SSH viewer split was not closed on exit')
      assert(closed_splits.viewer2 == 1, 'unconfirmed racing split was not closed on exit')
      assert(closed_splits.viewer3 == 1, 'failed rollback lost its owned viewer')
      assert(#quit_requests == 1, 'VimLeavePre did not send quit over forward_socket')
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
      if request.word ~= nil and request.word ~= vim.NIL then
        assert(request.word.words[request.word.selected + 1] == case.word,
          'best-guess forward hinted the wrong included-source word')
      end
      assert(request.best_guess == true and type(request.warning) == 'string' and request.warning ~= '',
        'best-guess navigation must retain its unverified ownership warning')
      assert(request.width > 0 or request.height > 0, 'forward lost usable compiler line geometry')
    end
  end
  do
    -- Public commands must choose a live directive before saving, while keeping
    -- the selected PDF's compiled anchors alive across that save.
    local base = directory .. '/edited root intent'
    vim.fn.mkdir(base .. '/.git', 'p')
    local child, main_a, main_b = base .. '/child.tex', base .. '/main-a.tex', base .. '/main-b.tex'
    local pdf_a, pdf_b = base .. '/main-a.pdf', base .. '/main-b.pdf'
    local literal = 'Copper owner needle glows among quiet amber stones.'
    local directive_a, directive_b = '% !TEX root = main-a.tex', '% !TEX root = main-b.tex'
    vim.fn.writefile({ directive_a, '', literal }, child)
    for _, main in ipairs({ main_a, main_b }) do
      local lines = {
        '\\documentclass{article}', '\\pagestyle{empty}', '\\begin{document}',
        main == main_a and 'Independent first root.' or 'Independent second root.',
        '\\newpage',
      }
      if main == main_b then
        vim.list_extend(lines, {
          'An extra root page distinguishes source ownership.', '\\newpage',
        })
      end
      lines[#lines + 1] = '\\input{child}'
      lines[#lines + 1] = '\\end{document}'
      vim.fn.writefile(lines, main)
    end
    local child_buffer = vim.fn.bufadd(child)
    vim.fn.bufload(child_buffer)
    local root_notices = {}
    vim.notify = function(message, level)
      root_notices[#root_notices + 1] = { message = tostring(message), level = level }
    end
    local function reset()
      vim.api.nvim_exec_autocmds('VimLeavePre', { group = 'pdfterm' })
      wait(function() return not vim.uv.fs_lstat(config.editor.path) end)
      package.loaded['pdfterm'] = nil
      adapter = require('pdfterm')
      adapter.setup({
        executable = wrapper, session = 'adapter', attach_only = true,
        focus_on_inverse = false, focus_on_forward = false, compile = true,
      })
    end
    local function finish_build()
      wait(function()
        for _, notice in ipairs(root_notices) do
          if notice.message:match('^Compilation OK') or notice.level == vim.log.levels.ERROR then
            return true
          end
        end
        return false
      end)
      local last = root_notices[#root_notices]
      assert(last.message:match('^Compilation OK'), last.message)
    end
    local function digest(path)
      local fd = assert(vim.uv.fs_open(path, 'r', 0))
      local bytes = assert(vim.uv.fs_read(fd, assert(vim.uv.fs_fstat(fd)).size, 0))
      assert(vim.uv.fs_close(fd))
      return vim.fn.sha256(bytes)
    end
    local function include(name)
      -- Use literal includes and change a recorded compiler input. A previously
      -- absent IfFileExists branch need not invalidate latexmk's old output.
      vim.api.nvim_buf_call(vim.fn.bufnr(main_b), function()
        vim.api.nvim_buf_set_lines(0, -2, -2, false, { '\\newpage', '\\input{' .. name .. '}' })
        vim.cmd('write')
      end)
    end
    local function navigate(pdf_path, page, word, row, text)
      vim.api.nvim_win_set_cursor(0, { row, assert(text:find(word, 1, true)) - 1 })
      local previous = #requests
      vim.cmd('PdfTermForward')
      wait(function() return #requests == previous + 1 end)
      local request = requests[#requests]
      assert(request.pdf == vim.uv.fs_realpath(pdf_path), 'live root selected the wrong PDF')
      assert(request.page == page,
        'root selection lost the included source cursor; expected page ' .. page .. ': '
          .. vim.json.encode(request))
      if request.word ~= nil and request.word ~= vim.NIL then
        assert(request.word.words[request.word.selected + 1] == word,
          'best-guess forward hinted the wrong source word: ' .. vim.json.encode(request))
      end
      assert(request.best_guess == true and type(request.warning) == 'string' and request.warning ~= '',
        'unchanged source anchor must retain its unverified ownership warning')
      assert(request.width > 0 or request.height > 0, 'forward lost usable compiler line geometry')
      return request
    end
    reset()
    for _, main in ipairs({ main_a, main_b }) do
      vim.cmd.edit(vim.fn.fnameescape(main))
      root_notices = {}
      vim.cmd('PdfTermBuild')
      finish_build()
    end
    local original_a, original_b = digest(pdf_a), digest(pdf_b)
    local raw = vim.system({
      binary, pdf_b, '--synctex-view', child, '--line', '3',
      '--column', tostring(assert(literal:find('needle', 1, true))),
    }, { text = true }):wait(10000)
    assert(raw.code == 0, raw.stderr)
    local expected = vim.json.decode(raw.stdout)
    vim.cmd.edit(vim.fn.fnameescape(child))
    adapter.toggle_compile()
    vim.api.nvim_buf_set_lines(child_buffer, 0, 1, false, { directive_b })
    vim.api.nvim_buf_set_lines(child_buffer, 2, 2, false, { '% A new comment moves only the live cursor.' })
    local describe, describes = project.describe, 0
    project.describe = function(...)
      describes = describes + 1
      return describe(...)
    end
    local ok_nav, request = pcall(navigate, pdf_b, 3, 'needle', 4, literal)
    project.describe = describe
    assert(ok_nav, request)
    assert(describes == 1, 'unmutated modified TeX save re-ran project.describe: ' .. describes)
    assert(request.h == expected.h and request.v == expected.v,
      'saving an edited directive rebased an unchanged compiled source span')
    assert(vim.fn.readfile(child)[1] == directive_b, 'forward did not save its selected source')
    assert(digest(pdf_a) == original_a and digest(pdf_b) == original_b,
      'noncompiling forward rebuilt a root')
    vim.api.nvim_buf_set_lines(child_buffer, 2, 3, false, { '% Edited before write-hook root mutation.' })
    vim.api.nvim_create_autocmd('BufWritePre', {
      buffer = child_buffer,
      once = true,
      callback = function()
        vim.api.nvim_buf_set_lines(child_buffer, 0, 1, false, { directive_a })
      end,
    })
    describes, root_notices = 0, {}
    project.describe = function(...)
      describes = describes + 1
      return describe(...)
    end
    vim.cmd('PdfTermForward')
    project.describe = describe
    assert(describes == 2, 'write-hook TeX mutation skipped post-save project.describe: ' .. describes)
    assert(#root_notices == 1
      and root_notices[1].message:find('source root changed while saving', 1, true),
      'write-hook root change was not rejected: ' .. vim.inspect(root_notices))
    vim.api.nvim_buf_set_lines(child_buffer, 0, 1, false, { directive_b })
    vim.cmd('write')

    -- Editing A to B must rebuild B, not merely resolve a B-shaped argument vector.
    vim.api.nvim_buf_set_lines(child_buffer, 0, 1, false, { directive_a })
    vim.cmd('write')
    vim.api.nvim_buf_set_lines(child_buffer, 0, 1, false, { directive_b })
    vim.api.nvim_buf_set_lines(child_buffer, -1, -1, false, {
      '', 'Newly printed turquoise lanterns distinguish the rebuilt second document.',
    })
    root_notices = {}
    vim.cmd('PdfTermBuild')
    finish_build()
    assert(vim.fn.readfile(child)[1] == directive_b, 'build saved the wrong source directive')
    assert(digest(pdf_a) == original_a and digest(pdf_b) ~= original_b,
      'public build compiled the old root after saving the new directive')

    -- Live standalone/included transitions must select the owner before saving.
    for _, standalone in ipairs({ false, true }) do
      local ownership = base .. '/live ownership ' .. tostring(standalone)
      vim.fn.mkdir(ownership .. '/nested', 'p')
      vim.fn.mkdir(ownership .. '/.git', 'p')
      local parent, changing = ownership .. '/parent.tex', ownership .. '/nested/child.tex'
      local prefix = {}
      for _ = 1, 25 do prefix[#prefix + 1] = '% Header comment.' end
      local function body(is_standalone)
        local lines = vim.deepcopy(prefix)
        if is_standalone then
          vim.list_extend(lines, { '\\documentclass{article}', '\\begin{document}' })
        end
        lines[#lines + 1] = 'Copper quartz identifies the selected source owner.'
        if is_standalone then lines[#lines + 1] = '\\end{document}' end
        return lines
      end
      vim.fn.writefile({
        '\\documentclass{article}', '\\begin{document}',
        '\\input{nested/child}', '\\end{document}',
      }, parent)
      vim.fn.writefile(body(standalone), changing)
      vim.cmd.edit(vim.fn.fnameescape(changing))
      root_notices = {}
      vim.cmd('PdfTermBuild')
      finish_build()
      local old_pdf = (standalone and changing or parent):gsub('%.tex$', '.pdf')
      local old_digest = digest(old_pdf)
      vim.api.nvim_buf_set_lines(0, 0, -1, false, body(not standalone))
      root_notices = {}
      vim.cmd('PdfTermBuild')
      finish_build()
      local selected_pdf = (standalone and parent or changing):gsub('%.tex$', '.pdf')
      assert(vim.fn.filereadable(selected_pdf) == 1, 'live ownership built the old root')
      assert(digest(old_pdf) == old_digest, 'live ownership rebuilt the independent old PDF')
      assert(not vim.bo.modified, 'ownership build did not save the selected source')
    end

    -- A new empty buffer is unmodified but still needs live ownership discovery.
    vim.fn.mkdir(base .. '/sub', 'p')
    local empty_child = base .. '/sub/zonk.tex'
    include('sub/zonk')
    vim.cmd.edit(vim.fn.fnameescape(empty_child))
    assert(not vim.bo.modified and not vim.uv.fs_stat(empty_child))
    root_notices = {}
    vim.cmd('PdfTermBuild')
    finish_build()
    assert(vim.fn.filereadable(empty_child) == 1, 'new unmodified child was not saved')
    assert(not vim.uv.fs_stat(base .. '/sub/zonk.pdf'), 'new unmodified child built its own root')
    vim.api.nvim_buf_call(vim.fn.bufnr(main_b), function()
      vim.api.nvim_buf_set_lines(0, -4, -2, false, {})
      vim.cmd('write')
    end)

    -- New unsaved children work with both a live directive and automatic ownership.
    local fresh_build = base .. '/fresh-build.tex'
    local fresh_literal = 'Silver owner silverneedle glows among quiet amber stones.'
    include('fresh-build')
    vim.cmd.edit(vim.fn.fnameescape(fresh_build))
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { directive_b, fresh_literal })
    assert(vim.fn.filereadable(fresh_build) == 0)
    root_notices = {}
    vim.cmd('PdfTermBuild')
    finish_build()
    assert(vim.fn.filereadable(fresh_build) == 1, 'new build source was not saved')
    navigate(pdf_b, 4, 'silverneedle', 2, fresh_literal)
    local fresh_forward = base .. '/fresh-forward.tex'
    local forward_literal = 'Violet grove orchard glows across mellow saffron hills.'
    include('fresh-forward')
    vim.cmd.edit(vim.fn.fnameescape(fresh_forward))
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { '% A new child without a directive.', forward_literal })
    assert(vim.fn.filereadable(fresh_forward) == 0)
    adapter.toggle_compile()
    navigate(pdf_b, 5, 'orchard', 2, forward_literal)
    assert(vim.fn.filereadable(fresh_forward) == 1, 'new forward source was not saved')
    assert(digest(pdf_a) == original_a, 'new included sources rebuilt the independent first root')

    adapter.toggle_compile()
    vim.cmd('PdfTermMain ' .. vim.fn.fnameescape(main_a))
    vim.cmd.edit(vim.fn.fnameescape(child))
    navigate(pdf_a, 2, 'needle', 4, literal)


    -- An unsaved standalone document may explicitly name itself through a
    -- parent-directory alias; no unrelated missing root gains that permission.
    local self_directory, self_alias = base .. '/self roots', base .. '/self alias'
    vim.fn.mkdir(self_directory)
    assert(vim.uv.fs_symlink(self_directory, self_alias))
    for _, case in ipairs({
      { name = 'new-build', action = 'build', target = 'buildneedle' },
      { name = 'new-forward', action = 'forward', target = 'forwardneedle' },
    }) do
      reset()
      local path = self_alias .. '/' .. case.name .. '.tex'
      local self_pdf = self_directory .. '/' .. case.name .. '.pdf'
      local self_literal = 'Copper owner glows ' .. case.target .. ' among quiet amber stones.'
      vim.cmd.edit(vim.fn.fnameescape(path))
      vim.api.nvim_buf_set_lines(0, 0, -1, false, {
        '% !TEX root = ./' .. case.name .. '.tex',
        '\\documentclass{article}', '\\pagestyle{empty}', '\\begin{document}',
        self_literal, '\\end{document}',
      })
      assert(vim.fn.filereadable(path) == 0 and not vim.uv.fs_stat(self_pdf))
      if case.action == 'build' then
        root_notices = {}
        vim.cmd('PdfTermBuild')
        finish_build()
        adapter.toggle_compile()
      end
      navigate(self_pdf, 1, case.target, 5, self_literal)
      assert(vim.fn.filereadable(path) == 1, 'self-root command did not perform the first save')
    end
    local missing_source = self_alias .. '/missing-child.tex'
    vim.cmd.edit(vim.fn.fnameescape(missing_source))
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { '% !TEX root = missing-other.tex' })
    root_notices = {}
    vim.cmd('PdfTermBuild')
    wait(function() return #root_notices > 0 end)
    assert(root_notices[#root_notices].level == vim.log.levels.ERROR
      and root_notices[#root_notices].message:find('TeX root file does not exist', 1, true),
      'a missing unrelated root was accepted as an unsaved self-root')
    assert(vim.fn.filereadable(missing_source) == 0, 'invalid root command saved its source')
    vim.bo.modified = false
    vim.notify = original_notify
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

  do
    local macro_dir = directory .. '/macro-include'
    vim.fn.mkdir(macro_dir, 'p')
    vim.fn.writefile({
      '\\documentclass{article}',
      '\\newcommand{\\includechapter}[1]{\\input{sub}}',
      '\\def\\myinclude{\\include{sub}}',
      '\\begin{document}',
      'Hello.',
      '\\end{document}',
    }, macro_dir .. '/unused.tex')
    vim.fn.writefile({ 'Subchapter.' }, macro_dir .. '/sub.tex')
    local resolved = project.describe(nil, macro_dir .. '/sub.tex')
    assert(resolved.main == vim.uv.fs_realpath(macro_dir .. '/sub.tex'),
      'unused macro definition falsely claimed project ownership: ' .. tostring(resolved.main))
    vim.fn.writefile({
      '\\documentclass{article}',
      '\\newcommand{\\bracketdefault}[1][{[}]{#1}',
      '\\newcommand{\\bareopen}[1][[]{#1}',
      '\\newenvironment{bareenv}[1][[]{}{}',
      '\\begin{document}',
      '\\input{sub}',
      '\\end{document}',
    }, macro_dir .. '/owner.tex')
    local owned = project.describe(nil, macro_dir .. '/sub.tex')
    assert(owned.main == vim.uv.fs_realpath(macro_dir .. '/owner.tex'),
      'braced bracket inside macro default hid subsequent \\input: ' .. tostring(owned.main))

  end

  -- Typst forward search compiles and maps directly through Tinymist without a
  -- redundant CLI pre-build, while :PdfTermBuild delegates to a retained service
  -- when present and falls back to the CLI build otherwise.
  do
    vim.api.nvim_exec_autocmds('VimLeavePre', { group = 'pdfterm' })
    wait(function() return not vim.uv.fs_lstat(config.editor.path) end)
    package.loaded['pdfterm'] = nil
    adapter = require('pdfterm')
    local path = directory .. '/synthetic.typ'
    vim.fn.writefile({ 'Zibble quarn.' }, path)
    vim.cmd.edit(vim.fn.fnameescape(path))
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'Flarn quarn.' })
    vim.api.nvim_create_autocmd('BufWritePre', {
      buffer = vim.api.nvim_get_current_buf(), once = true,
      callback = function() vim.api.nvim_buf_set_lines(0, 0, 0, false, { '// Plonk.' }) end,
    })
    adapter.setup({ executable = binary, session = 'adapter', attach_only = true,
      focus_on_inverse = false, focus_on_forward = false, compile = false })
    local typst = require('pdfterm.typst')
    local build, resolve, typst_build = project.build, typst.resolve, typst.build
    local builds, service_builds, resolutions, retained_service = 0, 0, 0, false
    project.build = function(built, _, callback)
      builds = builds + 1
      vim.fn.writefile({ '%PDF-1.4' }, built.pdf)
      if callback then callback({ code = 0 }) end
    end
    typst.build = function()
      if not retained_service then return false end
      service_builds = service_builds + 1
      return true
    end
    typst.resolve = function(_, _, _, _, callback)
      resolutions = resolutions + 1
      vim.schedule(function() callback({ code = 1, stderr = 'synthetic mapping stopped' }) end)
      return function() end
    end
    full_copies = 0
    vim.api.nvim_buf_get_lines = function(buffer, first, last, strict)
      if first == 0 and last == -1 then full_copies = full_copies + 1 end
      return get_lines(buffer, first, last, strict)
    end
    local checked, reason = xpcall(function()
      adapter.forward(false)
      wait(function() return resolutions == 1 end)
      assert(builds == 0, 'Typst forward with missing PDF ran a redundant CLI pre-build')
      assert(full_copies == 0, 'Typst save/navigation copied the full source buffer')
      assert(not vim.bo.modified and vim.fn.readfile(path)[1] == '// Plonk.',
        'Typst navigation skipped saving or its write hook')
      adapter.toggle_compile()
      adapter.forward(false)
      wait(function() return resolutions == 2 end)
      assert(builds == 0 and full_copies == 0, 'compile=true Typst forward ran a redundant CLI pre-build')
      adapter.build()
      wait(function() return builds == 1 end)
      assert(service_builds == 0, 'unretained Typst build skipped CLI build fallback')
      retained_service = true
      adapter.build()
      wait(function() return service_builds == 1 end)
      assert(builds == 1, 'retained Typst build raced a duplicate CLI build')
    end, debug.traceback)
    project.build, typst.resolve, typst.build = build, resolve, typst_build
    vim.api.nvim_buf_get_lines = get_lines
    assert(checked, reason)

    local latin1_path = directory .. '/latin1-unmodified.tex'
    local f = assert(io.open(latin1_path, 'wb'))
    f:write('% caf\233\n\\documentclass{article}\n\\begin{document}\nZibble.\n\\end{document}\n')
    f:close()
    vim.cmd.edit(vim.fn.fnameescape(latin1_path))
    assert(vim.bo.fileencoding == 'latin1' and not vim.bo.modified)
    local built_main
    project.build = function(built, _, callback)
      built_main = built.main
      callback({ code = 0 })
    end
    local ok_latin1, err_latin1 = xpcall(function()
      adapter.build()
      wait(function() return built_main ~= nil end)
      assert(built_main == vim.uv.fs_realpath(latin1_path))
    end, debug.traceback)
    project.build = build
    assert(ok_latin1, err_latin1)
  end

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
