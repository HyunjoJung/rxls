//! Canonical BIFF/XLSB built-in function table generated from [MS-XLSB] Ftab.
//!
//! Source: <https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/90a52fcb-ce63-497f-a3d3-173c42d82242>
//! A `None` arity means the function has an optional or repeated parameter grammar
//! and therefore must be represented by `PtgFuncVar`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Function {
    pub(crate) name: &'static str,
    pub(crate) fixed_arity: Option<usize>,
}

pub(crate) const FUNCTIONS: [Option<Function>; 485] = [
    Some(Function {
        name: "COUNT",
        fixed_arity: None,
    }), // 0x0000
    Some(Function {
        name: "IF",
        fixed_arity: None,
    }), // 0x0001
    Some(Function {
        name: "ISNA",
        fixed_arity: Some(1),
    }), // 0x0002
    Some(Function {
        name: "ISERROR",
        fixed_arity: Some(1),
    }), // 0x0003
    Some(Function {
        name: "SUM",
        fixed_arity: None,
    }), // 0x0004
    Some(Function {
        name: "AVERAGE",
        fixed_arity: None,
    }), // 0x0005
    Some(Function {
        name: "MIN",
        fixed_arity: None,
    }), // 0x0006
    Some(Function {
        name: "MAX",
        fixed_arity: None,
    }), // 0x0007
    Some(Function {
        name: "ROW",
        fixed_arity: None,
    }), // 0x0008
    Some(Function {
        name: "COLUMN",
        fixed_arity: None,
    }), // 0x0009
    Some(Function {
        name: "NA",
        fixed_arity: Some(0),
    }), // 0x000A
    Some(Function {
        name: "NPV",
        fixed_arity: None,
    }), // 0x000B
    Some(Function {
        name: "STDEV",
        fixed_arity: None,
    }), // 0x000C
    Some(Function {
        name: "DOLLAR",
        fixed_arity: None,
    }), // 0x000D
    Some(Function {
        name: "FIXED",
        fixed_arity: None,
    }), // 0x000E
    Some(Function {
        name: "SIN",
        fixed_arity: Some(1),
    }), // 0x000F
    Some(Function {
        name: "COS",
        fixed_arity: Some(1),
    }), // 0x0010
    Some(Function {
        name: "TAN",
        fixed_arity: Some(1),
    }), // 0x0011
    Some(Function {
        name: "ATAN",
        fixed_arity: Some(1),
    }), // 0x0012
    Some(Function {
        name: "PI",
        fixed_arity: Some(0),
    }), // 0x0013
    Some(Function {
        name: "SQRT",
        fixed_arity: Some(1),
    }), // 0x0014
    Some(Function {
        name: "EXP",
        fixed_arity: Some(1),
    }), // 0x0015
    Some(Function {
        name: "LN",
        fixed_arity: Some(1),
    }), // 0x0016
    Some(Function {
        name: "LOG10",
        fixed_arity: Some(1),
    }), // 0x0017
    Some(Function {
        name: "ABS",
        fixed_arity: Some(1),
    }), // 0x0018
    Some(Function {
        name: "INT",
        fixed_arity: Some(1),
    }), // 0x0019
    Some(Function {
        name: "SIGN",
        fixed_arity: Some(1),
    }), // 0x001A
    Some(Function {
        name: "ROUND",
        fixed_arity: Some(2),
    }), // 0x001B
    Some(Function {
        name: "LOOKUP",
        fixed_arity: None,
    }), // 0x001C
    Some(Function {
        name: "INDEX",
        fixed_arity: None,
    }), // 0x001D
    Some(Function {
        name: "REPT",
        fixed_arity: Some(2),
    }), // 0x001E
    Some(Function {
        name: "MID",
        fixed_arity: Some(3),
    }), // 0x001F
    Some(Function {
        name: "LEN",
        fixed_arity: Some(1),
    }), // 0x0020
    Some(Function {
        name: "VALUE",
        fixed_arity: Some(1),
    }), // 0x0021
    Some(Function {
        name: "TRUE",
        fixed_arity: Some(0),
    }), // 0x0022
    Some(Function {
        name: "FALSE",
        fixed_arity: Some(0),
    }), // 0x0023
    Some(Function {
        name: "AND",
        fixed_arity: None,
    }), // 0x0024
    Some(Function {
        name: "OR",
        fixed_arity: None,
    }), // 0x0025
    Some(Function {
        name: "NOT",
        fixed_arity: Some(1),
    }), // 0x0026
    Some(Function {
        name: "MOD",
        fixed_arity: Some(2),
    }), // 0x0027
    Some(Function {
        name: "DCOUNT",
        fixed_arity: Some(3),
    }), // 0x0028
    Some(Function {
        name: "DSUM",
        fixed_arity: Some(3),
    }), // 0x0029
    Some(Function {
        name: "DAVERAGE",
        fixed_arity: Some(3),
    }), // 0x002A
    Some(Function {
        name: "DMIN",
        fixed_arity: Some(3),
    }), // 0x002B
    Some(Function {
        name: "DMAX",
        fixed_arity: Some(3),
    }), // 0x002C
    Some(Function {
        name: "DSTDEV",
        fixed_arity: Some(3),
    }), // 0x002D
    Some(Function {
        name: "VAR",
        fixed_arity: None,
    }), // 0x002E
    Some(Function {
        name: "DVAR",
        fixed_arity: Some(3),
    }), // 0x002F
    Some(Function {
        name: "TEXT",
        fixed_arity: Some(2),
    }), // 0x0030
    Some(Function {
        name: "LINEST",
        fixed_arity: None,
    }), // 0x0031
    Some(Function {
        name: "TREND",
        fixed_arity: None,
    }), // 0x0032
    Some(Function {
        name: "LOGEST",
        fixed_arity: None,
    }), // 0x0033
    Some(Function {
        name: "GROWTH",
        fixed_arity: None,
    }), // 0x0034
    Some(Function {
        name: "GOTO",
        fixed_arity: Some(1),
    }), // 0x0035
    Some(Function {
        name: "HALT",
        fixed_arity: None,
    }), // 0x0036
    Some(Function {
        name: "RETURN",
        fixed_arity: None,
    }), // 0x0037
    Some(Function {
        name: "PV",
        fixed_arity: None,
    }), // 0x0038
    Some(Function {
        name: "FV",
        fixed_arity: None,
    }), // 0x0039
    Some(Function {
        name: "NPER",
        fixed_arity: None,
    }), // 0x003A
    Some(Function {
        name: "PMT",
        fixed_arity: None,
    }), // 0x003B
    Some(Function {
        name: "RATE",
        fixed_arity: None,
    }), // 0x003C
    Some(Function {
        name: "MIRR",
        fixed_arity: Some(3),
    }), // 0x003D
    Some(Function {
        name: "IRR",
        fixed_arity: None,
    }), // 0x003E
    Some(Function {
        name: "RAND",
        fixed_arity: Some(0),
    }), // 0x003F
    Some(Function {
        name: "MATCH",
        fixed_arity: None,
    }), // 0x0040
    Some(Function {
        name: "DATE",
        fixed_arity: Some(3),
    }), // 0x0041
    Some(Function {
        name: "TIME",
        fixed_arity: Some(3),
    }), // 0x0042
    Some(Function {
        name: "DAY",
        fixed_arity: Some(1),
    }), // 0x0043
    Some(Function {
        name: "MONTH",
        fixed_arity: Some(1),
    }), // 0x0044
    Some(Function {
        name: "YEAR",
        fixed_arity: Some(1),
    }), // 0x0045
    Some(Function {
        name: "WEEKDAY",
        fixed_arity: None,
    }), // 0x0046
    Some(Function {
        name: "HOUR",
        fixed_arity: Some(1),
    }), // 0x0047
    Some(Function {
        name: "MINUTE",
        fixed_arity: Some(1),
    }), // 0x0048
    Some(Function {
        name: "SECOND",
        fixed_arity: Some(1),
    }), // 0x0049
    Some(Function {
        name: "NOW",
        fixed_arity: Some(0),
    }), // 0x004A
    Some(Function {
        name: "AREAS",
        fixed_arity: Some(1),
    }), // 0x004B
    Some(Function {
        name: "ROWS",
        fixed_arity: Some(1),
    }), // 0x004C
    Some(Function {
        name: "COLUMNS",
        fixed_arity: Some(1),
    }), // 0x004D
    Some(Function {
        name: "OFFSET",
        fixed_arity: None,
    }), // 0x004E
    Some(Function {
        name: "ABSREF",
        fixed_arity: Some(2),
    }), // 0x004F
    Some(Function {
        name: "RELREF",
        fixed_arity: Some(2),
    }), // 0x0050
    Some(Function {
        name: "ARGUMENT",
        fixed_arity: None,
    }), // 0x0051
    Some(Function {
        name: "SEARCH",
        fixed_arity: None,
    }), // 0x0052
    Some(Function {
        name: "TRANSPOSE",
        fixed_arity: Some(1),
    }), // 0x0053
    Some(Function {
        name: "ERROR",
        fixed_arity: None,
    }), // 0x0054
    Some(Function {
        name: "STEP",
        fixed_arity: Some(0),
    }), // 0x0055
    Some(Function {
        name: "TYPE",
        fixed_arity: Some(1),
    }), // 0x0056
    Some(Function {
        name: "ECHO",
        fixed_arity: None,
    }), // 0x0057
    Some(Function {
        name: "SET.NAME",
        fixed_arity: None,
    }), // 0x0058
    Some(Function {
        name: "CALLER",
        fixed_arity: Some(0),
    }), // 0x0059
    Some(Function {
        name: "DEREF",
        fixed_arity: Some(1),
    }), // 0x005A
    Some(Function {
        name: "WINDOWS",
        fixed_arity: None,
    }), // 0x005B
    None, // 0x005C
    Some(Function {
        name: "DOCUMENTS",
        fixed_arity: None,
    }), // 0x005D
    Some(Function {
        name: "ACTIVE.CELL",
        fixed_arity: Some(0),
    }), // 0x005E
    Some(Function {
        name: "SELECTION",
        fixed_arity: Some(0),
    }), // 0x005F
    Some(Function {
        name: "RESULT",
        fixed_arity: None,
    }), // 0x0060
    Some(Function {
        name: "ATAN2",
        fixed_arity: Some(2),
    }), // 0x0061
    Some(Function {
        name: "ASIN",
        fixed_arity: Some(1),
    }), // 0x0062
    Some(Function {
        name: "ACOS",
        fixed_arity: Some(1),
    }), // 0x0063
    Some(Function {
        name: "CHOOSE",
        fixed_arity: None,
    }), // 0x0064
    Some(Function {
        name: "HLOOKUP",
        fixed_arity: None,
    }), // 0x0065
    Some(Function {
        name: "VLOOKUP",
        fixed_arity: None,
    }), // 0x0066
    Some(Function {
        name: "LINKS",
        fixed_arity: None,
    }), // 0x0067
    Some(Function {
        name: "INPUT",
        fixed_arity: None,
    }), // 0x0068
    Some(Function {
        name: "ISREF",
        fixed_arity: Some(1),
    }), // 0x0069
    Some(Function {
        name: "GET.FORMULA",
        fixed_arity: Some(1),
    }), // 0x006A
    Some(Function {
        name: "GET.NAME",
        fixed_arity: None,
    }), // 0x006B
    Some(Function {
        name: "SET.VALUE",
        fixed_arity: Some(2),
    }), // 0x006C
    Some(Function {
        name: "LOG",
        fixed_arity: None,
    }), // 0x006D
    Some(Function {
        name: "EXEC",
        fixed_arity: None,
    }), // 0x006E
    Some(Function {
        name: "CHAR",
        fixed_arity: Some(1),
    }), // 0x006F
    Some(Function {
        name: "LOWER",
        fixed_arity: Some(1),
    }), // 0x0070
    Some(Function {
        name: "UPPER",
        fixed_arity: Some(1),
    }), // 0x0071
    Some(Function {
        name: "PROPER",
        fixed_arity: Some(1),
    }), // 0x0072
    Some(Function {
        name: "LEFT",
        fixed_arity: None,
    }), // 0x0073
    Some(Function {
        name: "RIGHT",
        fixed_arity: None,
    }), // 0x0074
    Some(Function {
        name: "EXACT",
        fixed_arity: None,
    }), // 0x0075
    Some(Function {
        name: "TRIM",
        fixed_arity: Some(1),
    }), // 0x0076
    Some(Function {
        name: "REPLACE",
        fixed_arity: Some(4),
    }), // 0x0077
    Some(Function {
        name: "SUBSTITUTE",
        fixed_arity: None,
    }), // 0x0078
    Some(Function {
        name: "CODE",
        fixed_arity: Some(1),
    }), // 0x0079
    Some(Function {
        name: "NAMES",
        fixed_arity: None,
    }), // 0x007A
    Some(Function {
        name: "DIRECTORY",
        fixed_arity: None,
    }), // 0x007B
    Some(Function {
        name: "FIND",
        fixed_arity: None,
    }), // 0x007C
    Some(Function {
        name: "CELL",
        fixed_arity: None,
    }), // 0x007D
    Some(Function {
        name: "ISERR",
        fixed_arity: Some(1),
    }), // 0x007E
    Some(Function {
        name: "ISTEXT",
        fixed_arity: Some(1),
    }), // 0x007F
    Some(Function {
        name: "ISNUMBER",
        fixed_arity: Some(1),
    }), // 0x0080
    Some(Function {
        name: "ISBLANK",
        fixed_arity: Some(1),
    }), // 0x0081
    Some(Function {
        name: "T",
        fixed_arity: Some(1),
    }), // 0x0082
    Some(Function {
        name: "N",
        fixed_arity: Some(1),
    }), // 0x0083
    Some(Function {
        name: "FOPEN",
        fixed_arity: None,
    }), // 0x0084
    Some(Function {
        name: "FCLOSE",
        fixed_arity: Some(1),
    }), // 0x0085
    Some(Function {
        name: "FSIZE",
        fixed_arity: Some(1),
    }), // 0x0086
    Some(Function {
        name: "FREADLN",
        fixed_arity: Some(1),
    }), // 0x0087
    Some(Function {
        name: "FREAD",
        fixed_arity: Some(2),
    }), // 0x0088
    Some(Function {
        name: "FWRITELN",
        fixed_arity: Some(2),
    }), // 0x0089
    Some(Function {
        name: "FWRITE",
        fixed_arity: Some(2),
    }), // 0x008A
    Some(Function {
        name: "FPOS",
        fixed_arity: None,
    }), // 0x008B
    Some(Function {
        name: "DATEVALUE",
        fixed_arity: Some(1),
    }), // 0x008C
    Some(Function {
        name: "TIMEVALUE",
        fixed_arity: Some(1),
    }), // 0x008D
    Some(Function {
        name: "SLN",
        fixed_arity: Some(3),
    }), // 0x008E
    Some(Function {
        name: "SYD",
        fixed_arity: Some(4),
    }), // 0x008F
    Some(Function {
        name: "DDB",
        fixed_arity: None,
    }), // 0x0090
    Some(Function {
        name: "GET.DEF",
        fixed_arity: None,
    }), // 0x0091
    Some(Function {
        name: "REFTEXT",
        fixed_arity: None,
    }), // 0x0092
    Some(Function {
        name: "TEXTREF",
        fixed_arity: None,
    }), // 0x0093
    Some(Function {
        name: "INDIRECT",
        fixed_arity: None,
    }), // 0x0094
    Some(Function {
        name: "REGISTER",
        fixed_arity: None,
    }), // 0x0095
    Some(Function {
        name: "CALL",
        fixed_arity: None,
    }), // 0x0096
    Some(Function {
        name: "ADD.BAR",
        fixed_arity: None,
    }), // 0x0097
    Some(Function {
        name: "ADD.MENU",
        fixed_arity: None,
    }), // 0x0098
    Some(Function {
        name: "ADD.COMMAND",
        fixed_arity: None,
    }), // 0x0099
    Some(Function {
        name: "ENABLE.COMMAND",
        fixed_arity: None,
    }), // 0x009A
    Some(Function {
        name: "CHECK.COMMAND",
        fixed_arity: None,
    }), // 0x009B
    Some(Function {
        name: "RENAME.COMMAND",
        fixed_arity: None,
    }), // 0x009C
    Some(Function {
        name: "SHOW.BAR",
        fixed_arity: None,
    }), // 0x009D
    Some(Function {
        name: "DELETE.MENU",
        fixed_arity: None,
    }), // 0x009E
    Some(Function {
        name: "DELETE.COMMAND",
        fixed_arity: None,
    }), // 0x009F
    Some(Function {
        name: "GET.CHART.ITEM",
        fixed_arity: None,
    }), // 0x00A0
    Some(Function {
        name: "DIALOG.BOX",
        fixed_arity: Some(1),
    }), // 0x00A1
    Some(Function {
        name: "CLEAN",
        fixed_arity: Some(1),
    }), // 0x00A2
    Some(Function {
        name: "MDETERM",
        fixed_arity: Some(1),
    }), // 0x00A3
    Some(Function {
        name: "MINVERSE",
        fixed_arity: Some(1),
    }), // 0x00A4
    Some(Function {
        name: "MMULT",
        fixed_arity: Some(2),
    }), // 0x00A5
    Some(Function {
        name: "FILES",
        fixed_arity: None,
    }), // 0x00A6
    Some(Function {
        name: "IPMT",
        fixed_arity: None,
    }), // 0x00A7
    Some(Function {
        name: "PPMT",
        fixed_arity: None,
    }), // 0x00A8
    Some(Function {
        name: "COUNTA",
        fixed_arity: None,
    }), // 0x00A9
    Some(Function {
        name: "CANCEL.KEY",
        fixed_arity: None,
    }), // 0x00AA
    Some(Function {
        name: "FOR",
        fixed_arity: None,
    }), // 0x00AB
    Some(Function {
        name: "WHILE",
        fixed_arity: Some(1),
    }), // 0x00AC
    Some(Function {
        name: "BREAK",
        fixed_arity: Some(0),
    }), // 0x00AD
    Some(Function {
        name: "NEXT",
        fixed_arity: Some(0),
    }), // 0x00AE
    Some(Function {
        name: "INITIATE",
        fixed_arity: Some(2),
    }), // 0x00AF
    Some(Function {
        name: "REQUEST",
        fixed_arity: Some(2),
    }), // 0x00B0
    Some(Function {
        name: "POKE",
        fixed_arity: Some(3),
    }), // 0x00B1
    Some(Function {
        name: "EXECUTE",
        fixed_arity: Some(2),
    }), // 0x00B2
    Some(Function {
        name: "TERMINATE",
        fixed_arity: Some(1),
    }), // 0x00B3
    Some(Function {
        name: "RESTART",
        fixed_arity: None,
    }), // 0x00B4
    Some(Function {
        name: "HELP",
        fixed_arity: None,
    }), // 0x00B5
    Some(Function {
        name: "GET.BAR",
        fixed_arity: None,
    }), // 0x00B6
    Some(Function {
        name: "PRODUCT",
        fixed_arity: None,
    }), // 0x00B7
    Some(Function {
        name: "FACT",
        fixed_arity: Some(1),
    }), // 0x00B8
    Some(Function {
        name: "GET.CELL",
        fixed_arity: None,
    }), // 0x00B9
    Some(Function {
        name: "GET.WORKSPACE",
        fixed_arity: Some(1),
    }), // 0x00BA
    Some(Function {
        name: "GET.WINDOW",
        fixed_arity: None,
    }), // 0x00BB
    Some(Function {
        name: "GET.DOCUMENT",
        fixed_arity: None,
    }), // 0x00BC
    Some(Function {
        name: "DPRODUCT",
        fixed_arity: Some(3),
    }), // 0x00BD
    Some(Function {
        name: "ISNONTEXT",
        fixed_arity: Some(1),
    }), // 0x00BE
    Some(Function {
        name: "GET.NOTE",
        fixed_arity: None,
    }), // 0x00BF
    Some(Function {
        name: "NOTE",
        fixed_arity: None,
    }), // 0x00C0
    Some(Function {
        name: "STDEVP",
        fixed_arity: None,
    }), // 0x00C1
    Some(Function {
        name: "VARP",
        fixed_arity: None,
    }), // 0x00C2
    Some(Function {
        name: "DSTDEVP",
        fixed_arity: Some(3),
    }), // 0x00C3
    Some(Function {
        name: "DVARP",
        fixed_arity: Some(3),
    }), // 0x00C4
    Some(Function {
        name: "TRUNC",
        fixed_arity: None,
    }), // 0x00C5
    Some(Function {
        name: "ISLOGICAL",
        fixed_arity: Some(1),
    }), // 0x00C6
    Some(Function {
        name: "DCOUNTA",
        fixed_arity: Some(3),
    }), // 0x00C7
    Some(Function {
        name: "DELETE.BAR",
        fixed_arity: Some(1),
    }), // 0x00C8
    Some(Function {
        name: "UNREGISTER",
        fixed_arity: Some(1),
    }), // 0x00C9
    None, // 0x00CA
    None, // 0x00CB
    Some(Function {
        name: "USDOLLAR",
        fixed_arity: None,
    }), // 0x00CC
    Some(Function {
        name: "FINDB",
        fixed_arity: None,
    }), // 0x00CD
    Some(Function {
        name: "SEARCHB",
        fixed_arity: None,
    }), // 0x00CE
    Some(Function {
        name: "REPLACEB",
        fixed_arity: Some(4),
    }), // 0x00CF
    Some(Function {
        name: "LEFTB",
        fixed_arity: None,
    }), // 0x00D0
    Some(Function {
        name: "RIGHTB",
        fixed_arity: None,
    }), // 0x00D1
    Some(Function {
        name: "MIDB",
        fixed_arity: Some(3),
    }), // 0x00D2
    Some(Function {
        name: "LENB",
        fixed_arity: Some(3),
    }), // 0x00D3
    Some(Function {
        name: "ROUNDUP",
        fixed_arity: Some(2),
    }), // 0x00D4
    Some(Function {
        name: "ROUNDDOWN",
        fixed_arity: Some(2),
    }), // 0x00D5
    Some(Function {
        name: "ASC",
        fixed_arity: Some(1),
    }), // 0x00D6
    Some(Function {
        name: "DBCS",
        fixed_arity: Some(1),
    }), // 0x00D7
    Some(Function {
        name: "RANK",
        fixed_arity: None,
    }), // 0x00D8
    None, // 0x00D9
    None, // 0x00DA
    Some(Function {
        name: "ADDRESS",
        fixed_arity: None,
    }), // 0x00DB
    Some(Function {
        name: "DAYS360",
        fixed_arity: None,
    }), // 0x00DC
    Some(Function {
        name: "TODAY",
        fixed_arity: Some(0),
    }), // 0x00DD
    Some(Function {
        name: "VDB",
        fixed_arity: None,
    }), // 0x00DE
    Some(Function {
        name: "ELSE",
        fixed_arity: Some(0),
    }), // 0x00DF
    Some(Function {
        name: "ELSE.IF",
        fixed_arity: Some(1),
    }), // 0x00E0
    Some(Function {
        name: "END.IF",
        fixed_arity: Some(0),
    }), // 0x00E1
    Some(Function {
        name: "FOR.CELL",
        fixed_arity: None,
    }), // 0x00E2
    Some(Function {
        name: "MEDIAN",
        fixed_arity: None,
    }), // 0x00E3
    Some(Function {
        name: "SUMPRODUCT",
        fixed_arity: None,
    }), // 0x00E4
    Some(Function {
        name: "SINH",
        fixed_arity: Some(1),
    }), // 0x00E5
    Some(Function {
        name: "COSH",
        fixed_arity: Some(1),
    }), // 0x00E6
    Some(Function {
        name: "TANH",
        fixed_arity: Some(1),
    }), // 0x00E7
    Some(Function {
        name: "ASINH",
        fixed_arity: Some(1),
    }), // 0x00E8
    Some(Function {
        name: "ACOSH",
        fixed_arity: Some(1),
    }), // 0x00E9
    Some(Function {
        name: "ATANH",
        fixed_arity: Some(1),
    }), // 0x00EA
    Some(Function {
        name: "DGET",
        fixed_arity: Some(3),
    }), // 0x00EB
    Some(Function {
        name: "CREATE.OBJECT",
        fixed_arity: None,
    }), // 0x00EC
    Some(Function {
        name: "VOLATILE",
        fixed_arity: None,
    }), // 0x00ED
    Some(Function {
        name: "LAST.ERROR",
        fixed_arity: Some(0),
    }), // 0x00EE
    Some(Function {
        name: "CUSTOM.UNDO",
        fixed_arity: None,
    }), // 0x00EF
    Some(Function {
        name: "CUSTOM.REPEAT",
        fixed_arity: None,
    }), // 0x00F0
    Some(Function {
        name: "FORMULA.CONVERT",
        fixed_arity: None,
    }), // 0x00F1
    Some(Function {
        name: "GET.LINK.INFO",
        fixed_arity: None,
    }), // 0x00F2
    Some(Function {
        name: "TEXT.BOX",
        fixed_arity: None,
    }), // 0x00F3
    Some(Function {
        name: "INFO",
        fixed_arity: Some(1),
    }), // 0x00F4
    Some(Function {
        name: "GROUP",
        fixed_arity: Some(0),
    }), // 0x00F5
    Some(Function {
        name: "GET.OBJECT",
        fixed_arity: None,
    }), // 0x00F6
    Some(Function {
        name: "DB",
        fixed_arity: None,
    }), // 0x00F7
    Some(Function {
        name: "PAUSE",
        fixed_arity: None,
    }), // 0x00F8
    None, // 0x00F9
    None, // 0x00FA
    Some(Function {
        name: "RESUME",
        fixed_arity: None,
    }), // 0x00FB
    Some(Function {
        name: "FREQUENCY",
        fixed_arity: Some(2),
    }), // 0x00FC
    Some(Function {
        name: "ADD.TOOLBAR",
        fixed_arity: None,
    }), // 0x00FD
    Some(Function {
        name: "DELETE.TOOLBAR",
        fixed_arity: Some(1),
    }), // 0x00FE
    Some(Function {
        name: "user-defined function (UDF) or future function",
        fixed_arity: None,
    }), // 0x00FF
    Some(Function {
        name: "RESET.TOOLBAR",
        fixed_arity: Some(1),
    }), // 0x0100
    Some(Function {
        name: "EVALUATE",
        fixed_arity: Some(1),
    }), // 0x0101
    Some(Function {
        name: "GET.TOOLBAR",
        fixed_arity: None,
    }), // 0x0102
    Some(Function {
        name: "GET.TOOL",
        fixed_arity: None,
    }), // 0x0103
    Some(Function {
        name: "SPELLING.CHECK",
        fixed_arity: None,
    }), // 0x0104
    Some(Function {
        name: "ERROR.TYPE",
        fixed_arity: Some(1),
    }), // 0x0105
    Some(Function {
        name: "APP.TITLE",
        fixed_arity: None,
    }), // 0x0106
    Some(Function {
        name: "WINDOW.TITLE",
        fixed_arity: None,
    }), // 0x0107
    Some(Function {
        name: "SAVE.TOOLBAR",
        fixed_arity: None,
    }), // 0x0108
    Some(Function {
        name: "ENABLE.TOOL",
        fixed_arity: Some(3),
    }), // 0x0109
    Some(Function {
        name: "PRESS.TOOL",
        fixed_arity: Some(3),
    }), // 0x010A
    Some(Function {
        name: "REGISTER.ID",
        fixed_arity: None,
    }), // 0x010B
    Some(Function {
        name: "GET.WORKBOOK",
        fixed_arity: None,
    }), // 0x010C
    Some(Function {
        name: "AVEDEV",
        fixed_arity: None,
    }), // 0x010D
    Some(Function {
        name: "BETADIST",
        fixed_arity: None,
    }), // 0x010E
    Some(Function {
        name: "GAMMALN",
        fixed_arity: Some(1),
    }), // 0x010F
    Some(Function {
        name: "BETAINV",
        fixed_arity: None,
    }), // 0x0110
    Some(Function {
        name: "BINOMDIST",
        fixed_arity: Some(4),
    }), // 0x0111
    Some(Function {
        name: "CHIDIST",
        fixed_arity: Some(2),
    }), // 0x0112
    Some(Function {
        name: "CHIINV",
        fixed_arity: Some(2),
    }), // 0x0113
    Some(Function {
        name: "COMBIN",
        fixed_arity: Some(2),
    }), // 0x0114
    Some(Function {
        name: "CONFIDENCE",
        fixed_arity: Some(3),
    }), // 0x0115
    Some(Function {
        name: "CRITBINOM",
        fixed_arity: Some(3),
    }), // 0x0116
    Some(Function {
        name: "EVEN",
        fixed_arity: Some(1),
    }), // 0x0117
    Some(Function {
        name: "EXPONDIST",
        fixed_arity: Some(3),
    }), // 0x0118
    Some(Function {
        name: "FDIST",
        fixed_arity: Some(3),
    }), // 0x0119
    Some(Function {
        name: "FINV",
        fixed_arity: Some(3),
    }), // 0x011A
    Some(Function {
        name: "FISHER",
        fixed_arity: Some(1),
    }), // 0x011B
    Some(Function {
        name: "FISHERINV",
        fixed_arity: Some(1),
    }), // 0x011C
    Some(Function {
        name: "FLOOR",
        fixed_arity: Some(2),
    }), // 0x011D
    Some(Function {
        name: "GAMMADIST",
        fixed_arity: Some(4),
    }), // 0x011E
    Some(Function {
        name: "GAMMAINV",
        fixed_arity: Some(3),
    }), // 0x011F
    Some(Function {
        name: "CEILING",
        fixed_arity: Some(2),
    }), // 0x0120
    Some(Function {
        name: "HYPGEOMDIST",
        fixed_arity: Some(4),
    }), // 0x0121
    Some(Function {
        name: "LOGNORMDIST",
        fixed_arity: Some(3),
    }), // 0x0122
    Some(Function {
        name: "LOGINV",
        fixed_arity: Some(3),
    }), // 0x0123
    Some(Function {
        name: "NEGBINOMDIST",
        fixed_arity: Some(3),
    }), // 0x0124
    Some(Function {
        name: "NORMDIST",
        fixed_arity: Some(4),
    }), // 0x0125
    Some(Function {
        name: "NORMSDIST",
        fixed_arity: Some(1),
    }), // 0x0126
    Some(Function {
        name: "NORMINV",
        fixed_arity: Some(3),
    }), // 0x0127
    Some(Function {
        name: "NORMSINV",
        fixed_arity: Some(1),
    }), // 0x0128
    Some(Function {
        name: "STANDARDIZE",
        fixed_arity: Some(3),
    }), // 0x0129
    Some(Function {
        name: "ODD",
        fixed_arity: Some(1),
    }), // 0x012A
    Some(Function {
        name: "PERMUT",
        fixed_arity: Some(2),
    }), // 0x012B
    Some(Function {
        name: "POISSON",
        fixed_arity: Some(3),
    }), // 0x012C
    Some(Function {
        name: "TDIST",
        fixed_arity: Some(3),
    }), // 0x012D
    Some(Function {
        name: "WEIBULL",
        fixed_arity: Some(4),
    }), // 0x012E
    Some(Function {
        name: "SUMXMY2",
        fixed_arity: Some(2),
    }), // 0x012F
    Some(Function {
        name: "SUMX2MY2",
        fixed_arity: Some(2),
    }), // 0x0130
    Some(Function {
        name: "SUMX2PY2",
        fixed_arity: Some(2),
    }), // 0x0131
    Some(Function {
        name: "CHITEST",
        fixed_arity: Some(2),
    }), // 0x0132
    Some(Function {
        name: "CORREL",
        fixed_arity: Some(2),
    }), // 0x0133
    Some(Function {
        name: "COVAR",
        fixed_arity: Some(2),
    }), // 0x0134
    Some(Function {
        name: "FORECAST",
        fixed_arity: Some(3),
    }), // 0x0135
    Some(Function {
        name: "FTEST",
        fixed_arity: Some(2),
    }), // 0x0136
    Some(Function {
        name: "INTERCEPT",
        fixed_arity: Some(2),
    }), // 0x0137
    Some(Function {
        name: "PEARSON",
        fixed_arity: Some(2),
    }), // 0x0138
    Some(Function {
        name: "RSQ",
        fixed_arity: Some(2),
    }), // 0x0139
    Some(Function {
        name: "STEYX",
        fixed_arity: Some(2),
    }), // 0x013A
    Some(Function {
        name: "SLOPE",
        fixed_arity: Some(2),
    }), // 0x013B
    Some(Function {
        name: "TTEST",
        fixed_arity: Some(4),
    }), // 0x013C
    Some(Function {
        name: "PROB",
        fixed_arity: None,
    }), // 0x013D
    Some(Function {
        name: "DEVSQ",
        fixed_arity: None,
    }), // 0x013E
    Some(Function {
        name: "GEOMEAN",
        fixed_arity: None,
    }), // 0x013F
    Some(Function {
        name: "HARMEAN",
        fixed_arity: None,
    }), // 0x0140
    Some(Function {
        name: "SUMSQ",
        fixed_arity: None,
    }), // 0x0141
    Some(Function {
        name: "KURT",
        fixed_arity: None,
    }), // 0x0142
    Some(Function {
        name: "SKEW",
        fixed_arity: None,
    }), // 0x0143
    Some(Function {
        name: "ZTEST",
        fixed_arity: None,
    }), // 0x0144
    Some(Function {
        name: "LARGE",
        fixed_arity: Some(2),
    }), // 0x0145
    Some(Function {
        name: "SMALL",
        fixed_arity: Some(2),
    }), // 0x0146
    Some(Function {
        name: "QUARTILE",
        fixed_arity: Some(2),
    }), // 0x0147
    Some(Function {
        name: "PERCENTILE",
        fixed_arity: Some(2),
    }), // 0x0148
    Some(Function {
        name: "PERCENTRANK",
        fixed_arity: None,
    }), // 0x0149
    Some(Function {
        name: "MODE",
        fixed_arity: None,
    }), // 0x014A
    Some(Function {
        name: "TRIMMEAN",
        fixed_arity: Some(2),
    }), // 0x014B
    Some(Function {
        name: "TINV",
        fixed_arity: Some(2),
    }), // 0x014C
    None, // 0x014D
    Some(Function {
        name: "MOVIE.COMMAND",
        fixed_arity: None,
    }), // 0x014E
    Some(Function {
        name: "GET.MOVIE",
        fixed_arity: None,
    }), // 0x014F
    Some(Function {
        name: "CONCATENATE",
        fixed_arity: None,
    }), // 0x0150
    Some(Function {
        name: "POWER",
        fixed_arity: Some(2),
    }), // 0x0151
    Some(Function {
        name: "PIVOT.ADD.DATA",
        fixed_arity: None,
    }), // 0x0152
    Some(Function {
        name: "GET.PIVOT.TABLE",
        fixed_arity: None,
    }), // 0x0153
    Some(Function {
        name: "GET.PIVOT.FIELD",
        fixed_arity: None,
    }), // 0x0154
    Some(Function {
        name: "GET.PIVOT.ITEM",
        fixed_arity: None,
    }), // 0x0155
    Some(Function {
        name: "RADIANS",
        fixed_arity: Some(1),
    }), // 0x0156
    Some(Function {
        name: "DEGREES",
        fixed_arity: Some(1),
    }), // 0x0157
    Some(Function {
        name: "SUBTOTAL",
        fixed_arity: None,
    }), // 0x0158
    Some(Function {
        name: "SUMIF",
        fixed_arity: None,
    }), // 0x0159
    Some(Function {
        name: "COUNTIF",
        fixed_arity: Some(2),
    }), // 0x015A
    Some(Function {
        name: "COUNTBLANK",
        fixed_arity: Some(1),
    }), // 0x015B
    Some(Function {
        name: "SCENARIO.GET",
        fixed_arity: None,
    }), // 0x015C
    Some(Function {
        name: "OPTIONS.LISTS.GET",
        fixed_arity: Some(1),
    }), // 0x015D
    Some(Function {
        name: "ISPMT",
        fixed_arity: Some(4),
    }), // 0x015E
    Some(Function {
        name: "DATEDIF",
        fixed_arity: Some(3),
    }), // 0x015F
    Some(Function {
        name: "DATESTRING",
        fixed_arity: Some(1),
    }), // 0x0160
    Some(Function {
        name: "NUMBERSTRING",
        fixed_arity: Some(2),
    }), // 0x0161
    Some(Function {
        name: "ROMAN",
        fixed_arity: None,
    }), // 0x0162
    Some(Function {
        name: "OPEN.DIALOG",
        fixed_arity: None,
    }), // 0x0163
    Some(Function {
        name: "SAVE.DIALOG",
        fixed_arity: None,
    }), // 0x0164
    Some(Function {
        name: "VIEW.GET",
        fixed_arity: None,
    }), // 0x0165
    Some(Function {
        name: "GETPIVOTDATA",
        fixed_arity: None,
    }), // 0x0166
    Some(Function {
        name: "HYPERLINK",
        fixed_arity: None,
    }), // 0x0167
    Some(Function {
        name: "PHONETIC",
        fixed_arity: Some(1),
    }), // 0x0168
    Some(Function {
        name: "AVERAGEA",
        fixed_arity: None,
    }), // 0x0169
    Some(Function {
        name: "MAXA",
        fixed_arity: None,
    }), // 0x016A
    Some(Function {
        name: "MINA",
        fixed_arity: None,
    }), // 0x016B
    Some(Function {
        name: "STDEVPA",
        fixed_arity: None,
    }), // 0x016C
    Some(Function {
        name: "VARPA",
        fixed_arity: None,
    }), // 0x016D
    Some(Function {
        name: "STDEVA",
        fixed_arity: None,
    }), // 0x016E
    Some(Function {
        name: "VARA",
        fixed_arity: None,
    }), // 0x016F
    Some(Function {
        name: "BAHTTEXT",
        fixed_arity: Some(1),
    }), // 0x0170
    Some(Function {
        name: "THAIDAYOFWEEK",
        fixed_arity: Some(1),
    }), // 0x0171
    Some(Function {
        name: "THAIDIGIT",
        fixed_arity: Some(1),
    }), // 0x0172
    Some(Function {
        name: "THAIMONTHOFYEAR",
        fixed_arity: Some(1),
    }), // 0x0173
    Some(Function {
        name: "THAINUMSOUND",
        fixed_arity: Some(1),
    }), // 0x0174
    Some(Function {
        name: "THAINUMSTRING",
        fixed_arity: Some(1),
    }), // 0x0175
    Some(Function {
        name: "THAISTRINGLENGTH",
        fixed_arity: Some(1),
    }), // 0x0176
    Some(Function {
        name: "ISTHAIDIGIT",
        fixed_arity: Some(1),
    }), // 0x0177
    Some(Function {
        name: "ROUNDBAHTDOWN",
        fixed_arity: Some(1),
    }), // 0x0178
    Some(Function {
        name: "ROUNDBAHTUP",
        fixed_arity: Some(1),
    }), // 0x0179
    Some(Function {
        name: "THAIYEAR",
        fixed_arity: Some(1),
    }), // 0x017A
    Some(Function {
        name: "RTD",
        fixed_arity: None,
    }), // 0x017B
    Some(Function {
        name: "CUBEVALUE",
        fixed_arity: None,
    }), // 0x017C
    Some(Function {
        name: "CUBEMEMBER",
        fixed_arity: None,
    }), // 0x017D
    Some(Function {
        name: "CUBEMEMBERPROPERTY",
        fixed_arity: Some(3),
    }), // 0x017E
    Some(Function {
        name: "CUBERANKEDMEMBER",
        fixed_arity: None,
    }), // 0x017F
    Some(Function {
        name: "HEX2BIN",
        fixed_arity: None,
    }), // 0x0180
    Some(Function {
        name: "HEX2DEC",
        fixed_arity: Some(1),
    }), // 0x0181
    Some(Function {
        name: "HEX2OCT",
        fixed_arity: None,
    }), // 0x0182
    Some(Function {
        name: "DEC2BIN",
        fixed_arity: None,
    }), // 0x0183
    Some(Function {
        name: "DEC2HEX",
        fixed_arity: None,
    }), // 0x0184
    Some(Function {
        name: "DEC2OCT",
        fixed_arity: None,
    }), // 0x0185
    Some(Function {
        name: "OCT2BIN",
        fixed_arity: None,
    }), // 0x0186
    Some(Function {
        name: "OCT2HEX",
        fixed_arity: None,
    }), // 0x0187
    Some(Function {
        name: "OCT2DEC",
        fixed_arity: Some(1),
    }), // 0x0188
    Some(Function {
        name: "BIN2DEC",
        fixed_arity: Some(1),
    }), // 0x0189
    Some(Function {
        name: "BIN2OCT",
        fixed_arity: None,
    }), // 0x018A
    Some(Function {
        name: "BIN2HEX",
        fixed_arity: None,
    }), // 0x018B
    Some(Function {
        name: "IMSUB",
        fixed_arity: Some(2),
    }), // 0x018C
    Some(Function {
        name: "IMDIV",
        fixed_arity: Some(2),
    }), // 0x018D
    Some(Function {
        name: "IMPOWER",
        fixed_arity: Some(2),
    }), // 0x018E
    Some(Function {
        name: "IMABS",
        fixed_arity: Some(1),
    }), // 0x018F
    Some(Function {
        name: "IMSQRT",
        fixed_arity: Some(1),
    }), // 0x0190
    Some(Function {
        name: "IMLN",
        fixed_arity: Some(1),
    }), // 0x0191
    Some(Function {
        name: "IMLOG2",
        fixed_arity: Some(1),
    }), // 0x0192
    Some(Function {
        name: "IMLOG10",
        fixed_arity: Some(1),
    }), // 0x0193
    Some(Function {
        name: "IMSIN",
        fixed_arity: Some(1),
    }), // 0x0194
    Some(Function {
        name: "IMCOS",
        fixed_arity: Some(1),
    }), // 0x0195
    Some(Function {
        name: "IMEXP",
        fixed_arity: Some(1),
    }), // 0x0196
    Some(Function {
        name: "IMARGUMENT",
        fixed_arity: Some(1),
    }), // 0x0197
    Some(Function {
        name: "IMCONJUGATE",
        fixed_arity: Some(1),
    }), // 0x0198
    Some(Function {
        name: "IMAGINARY",
        fixed_arity: Some(1),
    }), // 0x0199
    Some(Function {
        name: "IMREAL",
        fixed_arity: Some(1),
    }), // 0x019A
    Some(Function {
        name: "COMPLEX",
        fixed_arity: None,
    }), // 0x019B
    Some(Function {
        name: "IMSUM",
        fixed_arity: None,
    }), // 0x019C
    Some(Function {
        name: "IMPRODUCT",
        fixed_arity: None,
    }), // 0x019D
    Some(Function {
        name: "SERIESSUM",
        fixed_arity: Some(4),
    }), // 0x019E
    Some(Function {
        name: "FACTDOUBLE",
        fixed_arity: Some(1),
    }), // 0x019F
    Some(Function {
        name: "SQRTPI",
        fixed_arity: Some(1),
    }), // 0x01A0
    Some(Function {
        name: "QUOTIENT",
        fixed_arity: Some(2),
    }), // 0x01A1
    Some(Function {
        name: "DELTA",
        fixed_arity: None,
    }), // 0x01A2
    Some(Function {
        name: "GESTEP",
        fixed_arity: None,
    }), // 0x01A3
    Some(Function {
        name: "ISEVEN",
        fixed_arity: Some(1),
    }), // 0x01A4
    Some(Function {
        name: "ISODD",
        fixed_arity: Some(1),
    }), // 0x01A5
    Some(Function {
        name: "MROUND",
        fixed_arity: Some(2),
    }), // 0x01A6
    Some(Function {
        name: "ERF",
        fixed_arity: None,
    }), // 0x01A7
    Some(Function {
        name: "ERFC",
        fixed_arity: Some(1),
    }), // 0x01A8
    Some(Function {
        name: "BESSELJ",
        fixed_arity: Some(2),
    }), // 0x01A9
    Some(Function {
        name: "BESSELK",
        fixed_arity: Some(2),
    }), // 0x01AA
    Some(Function {
        name: "BESSELY",
        fixed_arity: Some(2),
    }), // 0x01AB
    Some(Function {
        name: "BESSELI",
        fixed_arity: Some(2),
    }), // 0x01AC
    Some(Function {
        name: "XIRR",
        fixed_arity: None,
    }), // 0x01AD
    Some(Function {
        name: "XNPV",
        fixed_arity: Some(3),
    }), // 0x01AE
    Some(Function {
        name: "PRICEMAT",
        fixed_arity: None,
    }), // 0x01AF
    Some(Function {
        name: "YIELDMAT",
        fixed_arity: None,
    }), // 0x01B0
    Some(Function {
        name: "INTRATE",
        fixed_arity: None,
    }), // 0x01B1
    Some(Function {
        name: "RECEIVED",
        fixed_arity: None,
    }), // 0x01B2
    Some(Function {
        name: "DISC",
        fixed_arity: None,
    }), // 0x01B3
    Some(Function {
        name: "PRICEDISC",
        fixed_arity: None,
    }), // 0x01B4
    Some(Function {
        name: "YIELDDISC",
        fixed_arity: None,
    }), // 0x01B5
    Some(Function {
        name: "TBILLEQ",
        fixed_arity: Some(3),
    }), // 0x01B6
    Some(Function {
        name: "TBILLPRICE",
        fixed_arity: Some(3),
    }), // 0x01B7
    Some(Function {
        name: "TBILLYIELD",
        fixed_arity: Some(3),
    }), // 0x01B8
    Some(Function {
        name: "PRICE",
        fixed_arity: None,
    }), // 0x01B9
    Some(Function {
        name: "YIELD",
        fixed_arity: None,
    }), // 0x01BA
    Some(Function {
        name: "DOLLARDE",
        fixed_arity: Some(2),
    }), // 0x01BB
    Some(Function {
        name: "DOLLARFR",
        fixed_arity: Some(2),
    }), // 0x01BC
    Some(Function {
        name: "NOMINAL",
        fixed_arity: Some(2),
    }), // 0x01BD
    Some(Function {
        name: "EFFECT",
        fixed_arity: Some(2),
    }), // 0x01BE
    Some(Function {
        name: "CUMPRINC",
        fixed_arity: Some(6),
    }), // 0x01BF
    Some(Function {
        name: "CUMIPMT",
        fixed_arity: Some(6),
    }), // 0x01C0
    Some(Function {
        name: "EDATE",
        fixed_arity: Some(2),
    }), // 0x01C1
    Some(Function {
        name: "EOMONTH",
        fixed_arity: Some(2),
    }), // 0x01C2
    Some(Function {
        name: "YEARFRAC",
        fixed_arity: None,
    }), // 0x01C3
    Some(Function {
        name: "COUPDAYBS",
        fixed_arity: None,
    }), // 0x01C4
    Some(Function {
        name: "COUPDAYS",
        fixed_arity: None,
    }), // 0x01C5
    Some(Function {
        name: "COUPDAYSNC",
        fixed_arity: None,
    }), // 0x01C6
    Some(Function {
        name: "COUPNCD",
        fixed_arity: None,
    }), // 0x01C7
    Some(Function {
        name: "COUPNUM",
        fixed_arity: None,
    }), // 0x01C8
    Some(Function {
        name: "COUPPCD",
        fixed_arity: None,
    }), // 0x01C9
    Some(Function {
        name: "DURATION",
        fixed_arity: None,
    }), // 0x01CA
    Some(Function {
        name: "MDURATION",
        fixed_arity: None,
    }), // 0x01CB
    Some(Function {
        name: "ODDLPRICE",
        fixed_arity: None,
    }), // 0x01CC
    Some(Function {
        name: "ODDLYIELD",
        fixed_arity: None,
    }), // 0x01CD
    Some(Function {
        name: "ODDFPRICE",
        fixed_arity: None,
    }), // 0x01CE
    Some(Function {
        name: "ODDFYIELD",
        fixed_arity: None,
    }), // 0x01CF
    Some(Function {
        name: "RANDBETWEEN",
        fixed_arity: Some(2),
    }), // 0x01D0
    Some(Function {
        name: "WEEKNUM",
        fixed_arity: None,
    }), // 0x01D1
    Some(Function {
        name: "AMORDEGRC",
        fixed_arity: None,
    }), // 0x01D2
    Some(Function {
        name: "AMORLINC",
        fixed_arity: None,
    }), // 0x01D3
    None, // 0x01D4
    Some(Function {
        name: "ACCRINT",
        fixed_arity: None,
    }), // 0x01D5
    Some(Function {
        name: "ACCRINTM",
        fixed_arity: None,
    }), // 0x01D6
    Some(Function {
        name: "WORKDAY",
        fixed_arity: None,
    }), // 0x01D7
    Some(Function {
        name: "NETWORKDAYS",
        fixed_arity: None,
    }), // 0x01D8
    Some(Function {
        name: "GCD",
        fixed_arity: None,
    }), // 0x01D9
    Some(Function {
        name: "MULTINOMIAL",
        fixed_arity: None,
    }), // 0x01DA
    Some(Function {
        name: "LCM",
        fixed_arity: None,
    }), // 0x01DB
    Some(Function {
        name: "FVSCHEDULE",
        fixed_arity: Some(2),
    }), // 0x01DC
    Some(Function {
        name: "CUBEKPIMEMBER",
        fixed_arity: None,
    }), // 0x01DD
    Some(Function {
        name: "CUBESET",
        fixed_arity: None,
    }), // 0x01DE
    Some(Function {
        name: "CUBESETCOUNT",
        fixed_arity: Some(1),
    }), // 0x01DF
    Some(Function {
        name: "IFERROR",
        fixed_arity: Some(2),
    }), // 0x01E0
    Some(Function {
        name: "COUNTIFS",
        fixed_arity: None,
    }), // 0x01E1
    Some(Function {
        name: "SUMIFS",
        fixed_arity: None,
    }), // 0x01E2
    Some(Function {
        name: "AVERAGEIF",
        fixed_arity: None,
    }), // 0x01E3
    Some(Function {
        name: "AVERAGEIFS",
        fixed_arity: None,
    }), // 0x01E4
];

pub(crate) fn function(id: u16) -> Option<Function> {
    FUNCTIONS.get(usize::from(id)).copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_ftab_has_every_assigned_xlsb_id() {
        let missing: Vec<usize> = FUNCTIONS
            .iter()
            .enumerate()
            .filter_map(|(id, function)| function.is_none().then_some(id))
            .collect();
        assert_eq!(missing, vec![92, 202, 203, 217, 218, 249, 250, 333, 468]);
        assert_eq!(FUNCTIONS.iter().flatten().count(), 476);
    }

    #[test]
    fn historically_mismapped_ids_match_the_official_table() {
        for (id, name, arity) in [
            (0x0018, "ABS", Some(1)),
            (0x0022, "TRUE", Some(0)),
            (0x0023, "FALSE", Some(0)),
            (0x004A, "NOW", Some(0)),
        ] {
            assert_eq!(
                function(id),
                Some(Function {
                    name,
                    fixed_arity: arity
                })
            );
        }
    }

    #[test]
    fn fixed_and_variable_parameter_classes_are_explicit() {
        assert_eq!(function(0x001B).unwrap().fixed_arity, Some(2)); // ROUND
        assert_eq!(function(0x00A5).unwrap().fixed_arity, Some(2)); // MMULT
        assert_eq!(function(0x0000).unwrap().fixed_arity, None); // COUNT
        assert_eq!(function(0x0001).unwrap().fixed_arity, None); // IF
    }
}
