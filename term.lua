if vim.g.neovide then
  vim.o.laststatus = 0
  vim.o.cmdheight = 0

  vim.cmd([[
    colorscheme lunaperche
    set background=dark
    terminal
    startinsert
    set scrolloff=9999
    autocmd BufLeave term://* quit
  ]])
end
