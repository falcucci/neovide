if vim.g.neovide_terminal_mode then
  vim.o.laststatus = 0
  vim.o.cmdheight = 0

  vim.g.neovide_padding_left = 0
  vim.cmd([[
    set termguicolors
    terminal
    startinsert
    autocmd BufLeave term://* quit
]])
end
