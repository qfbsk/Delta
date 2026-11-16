function '@solid' --description '在当前标签打开 solid 窗口'
    if not set -q argv[1]
        printf '%s\n' '@solid：要一个文件路径，例如 @solid "C:\路径\文件"' >&2
        return 2
    end
    if not set -q SOLID
        printf '%s\n' '@solid：只在 Delta 窗口里的 odd 中可用' >&2
        return 1
    end
    set -l path (path resolve -- $argv[1])
    if not set -q path[1]
        printf '%s\n' "@solid：路径看不懂：$argv[1]" >&2
        return 2
    end
    command $SOLID --file $path
end
