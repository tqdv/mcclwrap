# Notes

## TODO

- Send all output to a function because we're handling user input

scripting languages:
- Python
- JS
- PHP
- Perl
- Ruby
- Rust
- Raku

filter-output | ={ <n>,<n> =#<re>#= }= | <cmd>
              | =#<re>#=               | 

guard start ={ save-off =;= save-all }= ={ save-on =|= save-all }= save-on
guard resume
```
// defaults to 30 seconds
guard prevent =(60)= =#^save-#=
slash save-off
get-line =#^Saved the game$#= =(,)= save-all
&do-backup
// guard renew
slash save-on
guard done
```
