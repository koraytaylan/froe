/*
 * Lucene's own transducer reader, over transducers froe wrote.
 *
 * froe's `.tip` writer emits a subset of what Lucene's builder does —
 * linear arcs only, never the fixed-array form — and the reader dispatches
 * on that per node. So "froe's bytes parse" is not the claim: the claim is
 * that Lucene enumerates exactly the key/output pairs froe put in, which is
 * what a terms index has to do for a seek to find a block.
 *
 * Reads `crates/froe/tests/fixtures/lucene-fst-corpus.tsv`, whose rows are
 * `<name>\t<fst bytes in hex>\t<key>=<output>,…`, with keys and outputs in
 * hex and an empty one written as an empty field. For each row it loads the
 * bytes with `FST` and `ByteSequenceOutputs`, enumerates every pair with
 * Lucene's own `BytesRefFSTEnum`, and prints what it found in the same
 * shape — so the caller compares rather than trusting a verdict computed
 * here.
 */
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;

import org.apache.lucene.store.ByteArrayDataInput;
import org.apache.lucene.util.BytesRef;
import org.apache.lucene.util.fst.ByteSequenceOutputs;
import org.apache.lucene.util.fst.BytesRefFSTEnum;
import org.apache.lucene.util.fst.FST;

public final class FstCheck {

    public static void main(String[] arguments) throws Exception {
        if (arguments.length < 2 || !arguments[0].equals("fst-check")) {
            StoreSupport.refuse("usage: FstCheck fst-check <corpus>");
            return;
        }
        for (String line : Files.readAllLines(Paths.get(arguments[1]), StandardCharsets.UTF_8)) {
            if (line.startsWith("#") || line.trim().isEmpty()) {
                continue;
            }
            String[] fields = line.split("\t", -1);
            if (fields.length < 2) {
                StoreSupport.refuse("a corpus row needs a name and bytes: " + line);
                return;
            }
            System.out.println(fields[0] + "\t" + enumerate(decode(fields[1])));
        }
    }

    /** Every pair Lucene finds, rendered as the corpus renders them. */
    private static String enumerate(byte[] serialized) throws IOException {
        FST<BytesRef> fst = new FST<BytesRef>(
                new ByteArrayDataInput(serialized), ByteSequenceOutputs.getSingleton());

        List<String> found = new ArrayList<String>();

        // The enumerator yields the empty key itself when the transducer
        // carries an empty output, so reading `getEmptyOutput` here as well
        // would report it twice.
        BytesRefFSTEnum<BytesRef> enumerator = new BytesRefFSTEnum<BytesRef>(fst);
        BytesRefFSTEnum.InputOutput<BytesRef> pair;
        while ((pair = enumerator.next()) != null) {
            found.add(hex(pair.input) + "=" + hex(pair.output));
        }
        return String.join(",", found);
    }

    private static String hex(BytesRef value) {
        if (value == null) {
            return "";
        }
        StringBuilder rendered = new StringBuilder();
        for (int i = 0; i < value.length; ++i) {
            rendered.append(String.format("%02x", value.bytes[value.offset + i]));
        }
        return rendered.toString();
    }

    private static byte[] decode(String hex) {
        byte[] bytes = new byte[hex.length() / 2];
        for (int i = 0; i < bytes.length; ++i) {
            bytes[i] = (byte) Integer.parseInt(hex.substring(2 * i, 2 * i + 2), 16);
        }
        return bytes;
    }
}
