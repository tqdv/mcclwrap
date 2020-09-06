# Notes

## Important

- Input filters can create deadlocks if they are not allowed to expire.
  eg. filterA block "b" waits for "a". filterB blocks "a" and waits for "b".
  A solution is to always match the command you expect: ie. filterA should also block "a",
  but it will be allowed because it's from the same client.
  This doesn't solve the problem if a filter is renewed indefinitely. So don't do that.

- I have no idea when a type needs to be Pin, Unpin, Send or Sync. They're usually added because the compiler complains
  about async functions not being Send. 

- `SharedOutputFilters` is a Vec instad of a HashSet/HashMap because that's faster.
  But `SharedInputFilters` is a HashMap because we need to remove our filter.

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
