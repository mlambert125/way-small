-- Project-local neovim config
-- Loaded automatically when vim.o.exrc = true (or via neoconf.nvim / nvim-project).
--
-- Requires: nvim-dap  (https://github.com/mfussenegger/nvim-dap)
-- Tools:    rust-analyzer, lldb-dap (both provided by the nix devshell — must be on PATH)

-- ---------------------------------------------------------------------------
-- LSP — rust-analyzer  (native vim.lsp API, nvim 0.11+)
-- ---------------------------------------------------------------------------
vim.lsp.config('rust_analyzer', {
    cmd          = { 'rust-analyzer' },
    filetypes    = { 'rust' },
    root_markers = { 'Cargo.toml', '.git' },
    settings     = {
        ['rust-analyzer'] = {
            cargo       = { allFeatures = true },
            checkOnSave = { command = 'clippy' },
        },
    },
})
vim.lsp.enable('rust_analyzer')

-- ---------------------------------------------------------------------------
-- DAP
-- ---------------------------------------------------------------------------
local ok, dap = pcall(require, 'dap')
if not ok then return end

-- ---------------------------------------------------------------------------
-- Adapter
-- ---------------------------------------------------------------------------
-- lldb-dap ships with llvm and is available inside `nix develop`.
dap.adapters.lldb = {
    type    = 'executable',
    command = 'lldb-dap',
    name    = 'lldb',
}

-- ---------------------------------------------------------------------------
-- Helper: resolve the debug executable for this project
-- ---------------------------------------------------------------------------
local function exe()
    local name = 'way-small'
    local path = vim.fn.getcwd() .. '/target/debug/' .. name
    if vim.fn.executable(path) == 0 then
        vim.notify(
            ('DAP: executable not found at %s\nRun `cargo build` first.'):format(path),
            vim.log.levels.WARN
        )
    end
    return path
end

-- ---------------------------------------------------------------------------
-- Keymaps
-- ---------------------------------------------------------------------------

-- Helper: run a command in a centered floating terminal.
local function run_script(cmd, label)
    local width  = math.floor(vim.o.columns * 0.8)
    local height = math.floor(vim.o.lines * 0.8)
    local buf    = vim.api.nvim_create_buf(false, true)
    vim.api.nvim_open_win(buf, true, {
        relative  = 'editor',
        width     = width,
        height    = height,
        col       = math.floor((vim.o.columns - width) / 2),
        row       = math.floor((vim.o.lines - height) / 2),
        style     = 'minimal',
        border    = 'rounded',
        title     = ' ' .. label .. ' ',
        title_pos = 'center',
    })
    vim.fn.termopen(cmd, {
        on_exit = function(_, code)
            if code == 0 then
                vim.notify(label .. ': done', vim.log.levels.INFO)
            else
                vim.notify(label .. ': failed (exit ' .. code .. ')', vim.log.levels.ERROR)
            end
        end,
    })
    vim.cmd('startinsert')
end

-- <leader>p* — project commands
vim.keymap.set('n', '<leader>pb', function() run_script('cargo build', 'build') end, { desc = 'Cargo: build' })
vim.keymap.set('n', '<leader>pr', function()
    vim.cmd('tabnew | terminal cargo run')
    vim.api.nvim_buf_set_name(0, 'cargo-run')
    vim.cmd('startinsert')
end, { desc = 'Cargo: run' })
vim.keymap.set('n', '<leader>pt', function() run_script('cargo test', 'test') end, { desc = 'Cargo: test' })
vim.keymap.set('n', '<leader>pc', function() run_script('cargo clean', 'clean') end, { desc = 'Cargo: clean' })
vim.keymap.set('n', '<leader>pl', function() run_script('cargo clippy', 'clippy') end, { desc = 'Cargo: clippy' })
vim.keymap.set('n', '<leader>pd', function() run_script('cargo doc --open', 'doc') end, { desc = 'Cargo: doc' })

-- ---------------------------------------------------------------------------
-- Launch configurations
-- ---------------------------------------------------------------------------
dap.configurations.rust = {
    {
        name        = 'Launch',
        type        = 'lldb',
        request     = 'launch',
        program     = exe,
        cwd         = '${workspaceFolder}',
        stopOnEntry = false,
        args        = {},
    },
    {
        name        = 'Launch (with args)',
        type        = 'lldb',
        request     = 'launch',
        program     = exe,
        cwd         = '${workspaceFolder}',
        stopOnEntry = false,
        args        = function()
            local raw = vim.fn.input('Args: ')
            return vim.split(raw, ' ', { trimempty = true })
        end,
    },
    {
        name    = 'Attach to PID',
        type    = 'lldb',
        request = 'attach',
        pid     = require('dap.utils').pick_process,
        cwd     = '${workspaceFolder}',
    },
}
