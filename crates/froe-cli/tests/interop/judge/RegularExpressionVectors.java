/*
 * What Oak's own name pattern answers for a pattern and a property path.
 *
 * `regular-expression-vectors <table>` reads a tab-separated table of
 * patterns and property paths and prints it back with a verdict column:
 * `match`, `nomatch`, or `invalid` when Java's own parser refuses the
 * pattern.
 *
 * The three lines below are `IndexDefinition.NamePattern`, which is a
 * private static class and so cannot be constructed from here: the
 * catch-all special case, Oak's own parent-and-name split through
 * `PathUtils`, and a **whole-string** match of the name expression
 * against the property's own name. A bare regular-expression match would
 * answer a different question.
 */
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.List;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

import org.apache.jackrabbit.oak.commons.PathUtils;
import org.apache.jackrabbit.oak.plugins.index.search.FulltextIndexConstants;

public final class RegularExpressionVectors {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length != 2 || !arguments[0].equals("regular-expression-vectors")) {
            StoreSupport.refuse("usage: RegularExpressionVectors regular-expression-vectors <table>");
            return;
        }
        List<String> lines = Files.readAllLines(Paths.get(arguments[1]), StandardCharsets.UTF_8);
        PrintWriter out = new PrintWriter(System.out);
        try {
            for (String line : lines) {
                if (line.startsWith("#") || line.isEmpty()) {
                    continue;
                }
                int tab = line.indexOf('\t');
                String pattern = line.substring(0, tab);
                String path = line.substring(tab + 1);
                out.println(escape(pattern) + "\t" + escape(path) + "\t" + verdict(pattern, path));
            }
        } finally {
            out.flush();
        }
    }

    /** `NamePattern.matches`, with `NamePattern`'s own constructor above it. */
    private static String verdict(String pattern, String propertyPath) {
        String parentPath;
        Pattern compiled;
        try {
            if (FulltextIndexConstants.REGEX_ALL_PROPS.equals(pattern)) {
                parentPath = "";
                compiled = Pattern.compile(pattern);
            } else {
                parentPath = PathUtils.getParentPath(pattern);
                compiled = Pattern.compile(PathUtils.getName(pattern));
            }
        } catch (PatternSyntaxException refused) {
            return "invalid";
        }
        if (!parentPath.equals(PathUtils.getParentPath(propertyPath))) {
            return "nomatch";
        }
        return compiled.matcher(PathUtils.getName(propertyPath)).matches() ? "match" : "nomatch";
    }

    private static String escape(String text) {
        return text.isEmpty() ? "\\e" : text;
    }
}
