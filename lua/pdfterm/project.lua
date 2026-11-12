-- Project builds are serialized; navigation generations begin at user intent.
local M = {}
local saved_source = require('pdfterm.source')
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

local function unsaved_identity(path)
  local parent = vim.uv.fs_realpath(vim.fs.dirname(path))
  return parent and parent .. '/' .. vim.fs.basename(path) or nil
end

-- TeXShop root directives name the owning document, not the source cursor.
local function root_source(source, current_source)
  local visited, explicit = {}, false
  while current_source ~= nil or vim.fn.filereadable(source) == 1 do
    local identity = vim.uv.fs_realpath(source) or unsaved_identity(source) or source
    local live = current_source ~= nil
    assert(not visited[identity], 'pdfterm: cyclic TeX root directive at ' .. source)
    visited[identity] = true
    local text, reason = current_source
    if text == nil then text, reason = saved_source.read_header(source, 65536) end
    current_source = nil
    assert(text, 'pdfterm: cannot read TeX root source: ' .. tostring(reason))
    local target, examined = nil, 0
    for line in vim.gsplit(text, '\n', { plain = true }) do
      examined = examined + 1
      if examined > 20 then break end
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
    local resolved = vim.uv.fs_realpath(target)
    if not resolved and live and unsaved_identity(target) == identity then
      return identity, true
    end
    target = assert(resolved, 'pdfterm: TeX root file does not exist: ' .. target)
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
local function tex_includes(path, text)
  if text == nil then text = saved_source.read_input(path) end
  if not text then
    return { inputs = {} }
  end
  local inputs, document = {}, false
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
  local function skip_bracket(pos)
    pos = whitespace(pos)
    if text:sub(pos, pos) ~= '[' then
      return pos, false
    end
    pos = pos + 1
    while pos <= #text do
      local char = text:sub(pos, pos)
      if char == '%' then
        pos = after_comment(pos)
      elseif char == '\\' then
        pos = pos + 2
      elseif char == '{' then
        _, pos = argument(pos, false)
      elseif char == ']' then
        return pos + 1, true
      else
        pos = pos + 1
      end
    end
    return pos, true
  end
  local function skip_definition(pos, command)
    if command == 'def' or command == 'gdef' or command == 'edef' or command == 'xdef' then
      while pos <= #text do
        local char = text:sub(pos, pos)
        if char == '%' then
          pos = after_comment(pos)
        elseif char == '\\' then
          pos = pos + 2
        elseif char == '{' then
          local _, next_pos = argument(pos, false)
          return next_pos
        else
          pos = pos + 1
        end
      end
      return pos
    end
    pos = whitespace(pos)
    if text:sub(pos, pos) == '*' then
      pos = whitespace(pos + 1)
    end
    if text:sub(pos, pos) == '{' then
      _, pos = argument(pos, false)
    elseif text:sub(pos, pos) == '\\' then
      local _, finish = text:find('^([%a@]+)', pos + 1)
      pos = finish and (finish + 1) or (pos + 2)
    end
    if command:find('DocumentCommand$', 1) then
      _, pos = argument(pos, false)
      _, pos = argument(pos, false)
      return pos
    end
    while true do
      local next_pos, skipped = skip_bracket(pos)
      pos = next_pos
      if not skipped then
        break
      end
    end
    _, pos = argument(pos, false)
    if command == 'newenvironment' or command == 'renewenvironment' then
      _, pos = argument(pos, false)
    end
    return pos
  end
  local pos = 1
  while pos <= #text do
    local next_pos = text:find('[%%\\]', pos)
    if not next_pos then
      break
    end
    pos = next_pos
    local char = text:sub(pos, pos)
    if char == '%' then
      pos = after_comment(pos)
    else
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
        elseif command == 'newcommand' or command == 'renewcommand' or command == 'providecommand'
          or command == 'DeclareRobustCommand' or command == 'NewDocumentCommand'
          or command == 'RenewDocumentCommand' or command == 'ProvideDocumentCommand'
          or command == 'DeclareDocumentCommand' or command == 'def' or command == 'gdef'
          or command == 'edef' or command == 'xdef' or command == 'newenvironment'
          or command == 'renewenvironment' then
          pos = skip_definition(pos, command)
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
    end
  end
  return { inputs = inputs, document = document }
end

local include_cache, root_cache = {}, {}

local function saved_includes(path)
  local revision = saved_source.saved_revision(vim.uv.fs_stat(path))
  local cached = include_cache[path]
  if revision and cached and vim.deep_equal(cached.revision, revision) then
    return cached.node, true
  end
  -- A stable byte read can be cached even when UTF-8 validation rejects it.
  -- Failed or concurrently truncated reads have no revision and remain uncached.
  local text, _, read_revision = saved_source.read_input(path)
  local node = text and tex_includes(path, text) or { inputs = {} }
  local stable = read_revision ~= nil
  include_cache[path] = stable and { revision = read_revision, node = node } or nil
  return node, stable
end

local function root_dependency(path)
  return { identity = vim.uv.fs_realpath(path) or false,
    revision = saved_source.saved_revision(vim.uv.fs_stat(path)) or false }
end

local function automatic_root(source, configured_cwd, current_source)
  local home = vim.uv.os_homedir()
  home = home and (vim.uv.fs_realpath(home) or home)
  local key = source .. '\0' .. (configured_cwd or '')
  local cached = current_source == nil and root_cache[key] or nil
  if cached and cached.home == home then
    local valid = true
    for path, value in pairs(cached.dependencies) do
      if not vim.deep_equal(value, root_dependency(path)) then valid = false; break end
    end
    if valid then return cached.root end
  end
  local dependencies, cacheable = {}, current_source == nil
  local function identity(path)
    local value = root_dependency(path)
    if dependencies[path] and not vim.deep_equal(dependencies[path], value) then cacheable = false end
    dependencies[path] = value
    return value.identity or nil
  end
  local function remember(root)
    if current_source == nil then
      root_cache[key] = cacheable and { root = root, home = home, dependencies = dependencies } or nil
    end
    return root
  end
  local target = identity(source)
  local unsaved = target == nil and current_source ~= nil
  target = target or unsaved_identity(source) or source
  local function included(path)
    local resolved = identity(path)
    if resolved or not unsaved then return resolved end
    -- A new child can already have a literal owner before its first write.
    return unsaved_identity(path) == target and target or nil
  end
  local parsed, directories = {}, {}
  local function scan(path)
    local resolved = identity(path) or (unsaved and path == target and target or nil)
    if not resolved then
      return nil
    end
    if not parsed[resolved] then
      if resolved == target and current_source ~= nil then
        parsed[resolved] = tex_includes(resolved, current_source)
      else
        local stable
        parsed[resolved], stable = saved_includes(resolved)
        cacheable = cacheable and stable
      end
    end
    return parsed[resolved], resolved
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
            child = included(name .. '.tex')
          end
          child = child or included(name)
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
    local directory_identity = identity(directory) or directory
    if directories[directory_identity] then
      break
    end
    directories[directory_identity] = true
    local owners, seen = {}, {}
    local entries = vim.uv.fs_scandir(directory)
    if not entries then cacheable = false end
    if entries then
      while true do
        local name = vim.uv.fs_scandir_next(entries)
        if not name then
          break
        end
        if name:match('%.tex$') then
          local candidate = vim.fs.normalize(directory .. '/' .. name)
          local canonical = identity(candidate)
          if canonical and not seen[canonical] then
            seen[canonical] = true
            if owns(canonical) then
              owners[#owners + 1] = canonical
            end
          end
        end
      end
    end
    if unsaved and directory == vim.fs.dirname(source) and not seen[target] then
      seen[target] = true
      if owns(target) then
        owners[#owners + 1] = target
      end
    end
    if #owners > 0 then
      table.sort(owners)
      assert(
        #owners == 1,
        'pdfterm: multiple TeX roots include ' .. source .. ': ' .. table.concat(owners, ', ')
          .. '; select one with :PdfTermMain or project.main'
      )
      return remember(owners[1])
    end
    -- Track absent boundaries too: creating one must invalidate an outer root.
    identity(directory .. '/.git')
    identity(directory .. '/.jj')
    if directory_identity == home or dependencies[directory .. '/.git'].revision
      or dependencies[directory .. '/.jj'].revision then
      break
    end
    local parent = vim.fs.dirname(directory)
    if parent == directory then
      break
    end
    directory = parent
  end
  return remember(source)
end

function M.describe(config, main, current_source)
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
    source, explicit = root_source(source, current_source)
    if not explicit then
      local cwd = p.cwd and vim.fs.normalize(vim.fn.fnamemodify(p.cwd, ':p'))
      source = automatic_root(source, cwd, current_source)
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

function M.build(project, id, callback, waiters)
  if not M.current(id) then
    for _, waiter in ipairs(waiters or {}) do waiter() end
    return
  end
  local job = { project = project, id = id, callback = callback, waiters = waiters }
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
  local function finish(result, status, quiet)
    if job.closed then return end
    local pending = job.pending
    builds[project.cwd], builds[project.pdf] = nil, nil
    completed = true
    if not quiet then
      show(
        status or (result.code == 0 and 'Compilation OK' or 'Compilation failed'),
        result.code == 0 and vim.log.levels.INFO or vim.log.levels.ERROR
      )
    end
    if callback and M.current(id) then callback(result) end
    if pending then
      local merged = job.waiters or {}
      for _, waiter in ipairs(pending.waiters or {}) do merged[#merged + 1] = waiter end
      M.build(pending.project, pending.id, pending.callback, #merged > 0 and merged or nil)
    elseif job.waiters then
      for _, waiter in ipairs(job.waiters) do waiter() end
    end
  end
  local function obsolete()
    if job.closed then return true end
    if M.current(id) then return false end
    finish({ code = 1, stdout = '', stderr = 'navigation cancelled' }, nil, true)
    return true
  end
  local function begin()
    if obsolete() then return end
    show('Compiling…', vim.log.levels.INFO)
    -- Notifications can synchronously cancel or supersede this build.
    if obsolete() then return end
    M.run(project.build, project.cwd, 120000, function(result)
      if job.closed then return end
      finish(result)
    end, function(data)
      append(data)
      if scheduled then return end
      scheduled = true
      vim.defer_fn(function()
        scheduled = false
        if not completed then show('Compiling…', vim.log.levels.INFO) end
      end, 100)
    end)
  end
  begin()
end

function M.after_build(project, callback)
  local active = builds[project.cwd] or builds[project.pdf]
  if not active then
    callback()
    return
  end
  active.waiters = active.waiters or {}
  active.waiters[#active.waiters + 1] = callback
end

function M.close()
  M.next()
  for _, job in pairs(builds) do
    job.pending = nil
    job.waiters = nil
    job.closed = true
  end
  for process in pairs(processes) do
    vim.uv.kill(-process.pid, 9)
  end
end
return M
