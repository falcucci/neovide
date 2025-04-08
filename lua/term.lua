if vim.g.neovide_terminal_mode then
  vim.o.laststatus = 0
  vim.o.cmdheight = 0
  vim.o.number = false

  vim.g.neovide_padding_left = 0

  vim.cmd([[
    set termguicolors
    autocmd TermOpen * setlocal nonumber norelativenumber
    autocmd VimEnter * terminal | startinsert
    terminal
    set nu!
    autocmd TermOpen * setlocal nonumber norelativenumber
    autocmd VimEnter * terminal | startinsert
    startinsert
    autocmd TermOpen * setlocal nonumber norelativenumber
    autocmd VimEnter * terminal | startinsert
    setlocal nonumber
    autocmd BufLeave term://* quit
]])
end
