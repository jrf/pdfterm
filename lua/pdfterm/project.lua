-- Project builds are serialized; navigation generations begin at user intent.
local M = {}
local generation, builds, processes = 0, {}, {}

function M.current(id)
  return id == generation
end
function M.next()
  generation = generation + 1
  return generation
end

-- A detached Unix process owns a process group, so timeouts also kill its children.
function M.run(argv, cwd, timeout, callback, on_output)
  local chunks, size, failure = { stdout = {}, stderr = {} }, 0, nil
  local process, timer, finished
  local function kill(reason)
    if finished then
      return
    end
    failure = reason
    if process then
      vim.uv.kill(-process.pid, 9)
    end
  end
  local function consume(stream)
    return function(error, data)
      if error then
        kill(tostring(error))
        return
      end
      if not data or failure then
        return
      end
      size = size + #data
      if size > 1024 * 1024 then
        kill('helper output exceeds 1 MiB')
      else
        chunks[stream][#chunks[stream] + 1] = data
        if on_output then
          on_output(data)
        end
      end
    end
  end
  local ok, spawned = pcall(vim.system, argv, {
    cwd = cwd,
    detach = true,
    stdout = consume('stdout'),
    stderr = consume('stderr'),
  }, function(result)
    finished = true
    if timer then
      timer:stop()
      timer:close()
    end
    if process then
      processes[process] = nil
      -- A completed parent must not leave descendants holding pipes or builds.
      vim.uv.kill(-process.pid, 9)
    end
    result.stdout, result.stderr = table.concat(chunks.stdout), failure or table.concat(chunks.stderr)
    if failure then
      result.code = 1
      if on_output then
        on_output('\n' .. failure)
      end
    end
    vim.schedule(function()
      callback(result)
    end)
  end)
  if not ok then
    vim.schedule(function()
      if on_output then
        on_output(tostring(spawned))
      end
      callback({ code = 1, stderr = tostring(spawned), stdout = '' })
    end)
    return function() end
  end
  process = spawned
  processes[process] = true
  timer = assert(vim.uv.new_timer())
  timer:start(timeout, 0, function()
    kill('helper timed out')
  end)
  return function()
    kill('navigation cancelled')
  end
end

-- TeXShop root directives name the owning document, not the source cursor.
local function root_source(source)
  local visited, explicit = {}, false
  while vim.fn.filereadable(source) == 1 do
    local identity = vim.uv.fs_realpath(source) or source
    assert(not visited[identity], 'pdfterm: cyclic TeX root directive at ' .. source)
    visited[identity] = true
    local target
    for _, line in ipairs(vim.fn.readfile(source, '', 20)) do
      target = line:match('^%s*%%%s*!%s*[Tt][Ee][Xx]%s+[Rr][Oo][Oo][Tt]%s*=%s*(.-)%s*$')
      if target then
        break
      end
    end
    if not target then
      return source, explicit
    end
    explicit = true
    target = target:match('^"(.*)"$') or target:match("^'(.*)'$") or target
    assert(target:match('%.tex$'), 'pdfterm: TeX root directive must name a .tex file in ' .. source)
    if not vim.startswith(target, '/') then
      target = vim.fs.dirname(source) .. '/' .. target
    end
    target = assert(vim.uv.fs_realpath(target), 'pdfterm: TeX root file does not exist: ' .. target)
    assert(vim.fn.filereadable(target) == 1, 'pdfterm: TeX root file is not readable: ' .. target)
    if target == identity then
      return target, true
    end
    source = target
  end
  return source, explicit
end

-- This is a literal source graph, not a TeX interpreter. In particular, macro
-- filenames, conditionals, search paths and catcode changes are not evaluated.
local function tex_includes(path)
  local ok, lines = pcall(vim.fn.readfile, path)
  if not ok then
    return { inputs = {} }
  end
  local text, inputs, document = table.concat(lines, '\n'), {}, false
  local function after_comment(pos)
    local newline = text:find('\n', pos, true)
    return newline and newline + 1 or #text + 1
  end
  local function whitespace(pos)
    while pos <= #text do
      local char = text:sub(pos, pos)
      if char == '%' then
        pos = after_comment(pos)
      elseif char:match('%s') then
        pos = pos + 1
      else
        break
      end
    end
    return pos
  end
  local function argument(pos, unbraced)
    pos = whitespace(pos)
    local first, value = text:sub(pos, pos), {}
    if first == '{' then
      local depth = 1
      pos = pos + 1
      while pos <= #text do
        local char = text:sub(pos, pos)
        if char == '%' then
          pos = after_comment(pos)
        elseif char == '\\' then
          value[#value + 1] = text:sub(pos, pos + 1)
          pos = pos + 2
        else
          if char == '{' then
            depth = depth + 1
          elseif char == '}' then
            depth = depth - 1
            if depth == 0 then
              return table.concat(value), pos + 1
            end
          end
          value[#value + 1] = char
          pos = pos + 1
        end
      end
    elseif unbraced then
      if first == '"' then
        local finish = text:find('"', pos + 1, true)
        if finish then
          return text:sub(pos + 1, finish - 1), finish + 1
        end
      else
        local finish = text:find('[%s%%{}]', pos)
        finish = finish or #text + 1
        return text:sub(pos, finish - 1), finish
      end
    end
    return nil, pos
  end
  local pos = 1
  while pos <= #text do
    local char = text:sub(pos, pos)
    if char == '%' then
      pos = after_comment(pos)
    elseif char == '\\' then
      local _, finish, command = text:find('^([%a@]+)', pos + 1)
      if not command then
        -- A control symbol consumes both characters: \\input is not \input.
        pos = pos + 2
      else
        pos = finish + 1
        if command == 'verb' then
          if text:sub(pos, pos) == '*' then
            pos = pos + 1
          end
          local delimiter = text:sub(pos, pos)
          local close = delimiter ~= '' and text:find(delimiter, pos + 1, true)
          local newline = text:find('\n', pos, true)
          pos = math.min(close or #text, newline or #text) + 1
        elseif command == 'begin' then
          local environment
          environment, pos = argument(pos, false)
          if environment == 'verbatim' or environment == 'verbatim*' or environment == 'Verbatim' then
            local _, close = text:find('\\end{' .. environment .. '}', pos, true)
            pos = close and close + 1 or #text + 1
          end
        elseif command == 'documentclass' then
          document = true
        elseif command == 'input' or command == 'include' or command == 'subfile' then
          local name
          name, pos = argument(pos, command == 'input')
          if name then
            name = vim.trim(name)
            name = name:match('^"(.*)"$') or name
            if name ~= '' and not name:find('[\\{}#$&^~%%"\r\n%z]') then
              inputs[#inputs + 1] = name
            end
          end
        end
      end
    else
      pos = pos + 1
    end
  end
  return { inputs = inputs, document = document }
end

local function automatic_root(source, configured_cwd)
  local target = vim.uv.fs_realpath(source) or source
  local home = vim.uv.os_homedir()
  home = home and (vim.uv.fs_realpath(home) or home)
  local parsed, directories = {}, {}
  local function scan(path)
    local identity = vim.uv.fs_realpath(path)
    if not identity then
      return nil
    end
    if not parsed[identity] then
      parsed[identity] = tex_includes(identity)
    end
    return parsed[identity], identity
  end
  local function owns(candidate)
    local root, identity = scan(candidate)
    if not root or not root.document then
      return false
    end
    local cwd = configured_cwd or vim.fs.dirname(candidate)
    local pending, visited = { identity }, {}
    while #pending > 0 do
      local path = table.remove(pending)
      if path == target then
        return true
      end
      if not visited[path] then
        visited[path] = true
        local node = scan(path)
        for _, name in ipairs(node and node.inputs or {}) do
          if not vim.startswith(name, '/') then
            name = cwd .. '/' .. name
          end
          local child
          if not name:match('%.tex$') then
            child = vim.uv.fs_realpath(name .. '.tex')
          end
          child = child or vim.uv.fs_realpath(name)
          if child and not visited[child] then
            pending[#pending + 1] = child
          end
        end
      end
    end
    return false
  end
  local directory = vim.fs.dirname(source)
  while directory do
    local identity = vim.uv.fs_realpath(directory) or directory
    if directories[identity] then
      break
    end
    directories[identity] = true
    local owners, seen = {}, {}
    local entries = vim.uv.fs_scandir(directory)
    if entries then
      while true do
        local name = vim.uv.fs_scandir_next(entries)
        if not name then
          break
        end
        if name:match('%.tex$') then
          local candidate = vim.fs.normalize(directory .. '/' .. name)
          local canonical = vim.uv.fs_realpath(candidate)
          if canonical and not seen[canonical] then
            seen[canonical] = true
            if owns(canonical) then
              owners[#owners + 1] = canonical
            end
          end
        end
      end
    end
    if #owners > 0 then
      table.sort(owners)
      assert(
        #owners == 1,
        'pdfterm: multiple TeX roots include ' .. source .. ': ' .. table.concat(owners, ', ')
          .. '; select one with :PdfTermMain or project.main'
      )
      return owners[1]
    end
    if identity == home or vim.uv.fs_stat(directory .. '/.git') or vim.uv.fs_stat(directory .. '/.jj') then
      break
    end
    local parent = vim.fs.dirname(directory)
    if parent == directory then
      break
    end
    directory = parent
  end
  return source
end

function M.describe(config, main)
  local p = config or {}
  local source = p.main or main
  assert(
    type(source) == 'string' and (source:match('%.tex$') or source:match('%.typ$')),
    'pdfterm: select a main TeX or Typst file with :PdfTermMain or project.main'
  )
  if p.cwd and not vim.startswith(source, '/') then
    source = vim.fn.fnamemodify(p.cwd, ':p') .. '/' .. source
  end
  source = vim.fs.normalize(vim.fn.fnamemodify(source, ':p'))
  local typst = source:match('%.typ$') ~= nil
  if not typst and not p.main then
    local explicit
    source, explicit = root_source(source)
    if not explicit then
      local cwd = p.cwd and vim.fs.normalize(vim.fn.fnamemodify(p.cwd, ':p'))
      source = automatic_root(source, cwd)
    end
  end
  local cwd = vim.fn.fnamemodify(p.cwd or vim.fs.dirname(source), ':p')
  cwd = assert(vim.uv.fs_realpath(cwd), 'pdfterm: project working directory does not exist')
  local pdf = p.pdf or source:gsub(typst and '%.typ$' or '%.tex$', '.pdf')
  if not vim.startswith(pdf, '/') then
    pdf = cwd .. '/' .. pdf
  end
  pdf = vim.fs.normalize(pdf)
  local argv = p.build
  if not argv then
    if typst then
      argv = { 'typst', 'compile', source, pdf }
    else
      local name = vim.fs.basename(pdf):match('^(.+)%.pdf$')
      assert(name, 'pdfterm: default LaTeX build requires a .pdf output file')
      argv = {
        'latexmk', '-pdf', '-interaction=nonstopmode', '-synctex=1',
        '-outdir=' .. vim.fs.dirname(pdf), '-jobname=' .. name, source,
      }
    end
  end
  assert(type(argv) == 'table' and #argv > 0, 'pdfterm: project.build must be a nonempty argument vector')
  for _, arg in ipairs(argv) do
    assert(type(arg) == 'string' and not arg:find('%z'), 'pdfterm: invalid build argument')
  end
  return { main = source, pdf = pdf, cwd = cwd, build = argv }
end

function M.build(project, id, callback)
  local job = { project = project, id = id, callback = callback }
  local active = builds[project.cwd] or builds[project.pdf]
  if active then
    active.pending = job
    return
  end
  builds[project.cwd], builds[project.pdf] = job, job
  local tail, scheduled, completed = '', false, false
  local notification = { id = 'pdfterm.build.' .. id, title = 'pdfterm: ' .. vim.fs.basename(project.pdf) }
  local function append(data)
    tail = (tail .. data:sub(-2000)):sub(-2000)
  end
  local function show(status, level)
    local lines = vim.split(tail:gsub('\r\n', '\n'):gsub('\r', '\n'):gsub('\n$', ''), '\n', { plain = true })
    local recent = table.concat(lines, '\n', math.max(1, #lines - 4))
    vim.notify(
      status .. (recent ~= '' and '\n' .. recent or ''),
      level,
      vim.tbl_extend('force', notification, {
        timeout = completed and 5000 or false,
      })
    )
  end
  show('Compiling…', vim.log.levels.INFO)
  M.run(project.build, project.cwd, 120000, function(result)
    local pending = job.pending
    builds[project.cwd], builds[project.pdf] = nil, nil
    completed = true
    show(
      result.code == 0 and 'Compilation OK' or 'Compilation failed',
      result.code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR
    )
    -- Never navigate from a superseded intent, even if the old build succeeds.
    if callback and M.current(id) then
      callback(result)
    end
    if pending then
      M.build(pending.project, pending.id, pending.callback)
    end
  end, function(data)
    append(data)
    if scheduled then
      return
    end
    scheduled = true
    vim.defer_fn(function()
      scheduled = false
      if not completed then
        show('Compiling…', vim.log.levels.INFO)
      end
    end, 100)
  end)
end

function M.close()
  M.next()
  for _, job in pairs(builds) do
    job.pending = nil
  end
  for process in pairs(processes) do
    vim.uv.kill(-process.pid, 9)
  end
end
return M
