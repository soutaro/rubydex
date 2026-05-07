#include "signature.h"
#include "location.h"

VALUE cSignature;
VALUE cParameter;
VALUE cPositionalParameter;
VALUE cOptionalPositionalParameter;
VALUE cRestPositionalParameter;
VALUE cPostParameter;
VALUE cKeywordParameter;
VALUE cOptionalKeywordParameter;
VALUE cRestKeywordParameter;
VALUE cForwardParameter;
VALUE cBlockParameter;

void rdxi_initialize_signature(VALUE mRubydex) {
    cSignature = rb_define_class_under(mRubydex, "Signature", rb_cObject);

    cParameter = rb_define_class_under(cSignature, "Parameter", rb_cObject);
    cPositionalParameter = rb_define_class_under(cSignature, "PositionalParameter", cParameter);
    cOptionalPositionalParameter = rb_define_class_under(cSignature, "OptionalPositionalParameter", cParameter);
    cRestPositionalParameter = rb_define_class_under(cSignature, "RestPositionalParameter", cParameter);
    cPostParameter = rb_define_class_under(cSignature, "PostParameter", cParameter);
    cKeywordParameter = rb_define_class_under(cSignature, "KeywordParameter", cParameter);
    cOptionalKeywordParameter = rb_define_class_under(cSignature, "OptionalKeywordParameter", cParameter);
    cRestKeywordParameter = rb_define_class_under(cSignature, "RestKeywordParameter", cParameter);
    cForwardParameter = rb_define_class_under(cSignature, "ForwardParameter", cParameter);
    cBlockParameter = rb_define_class_under(cSignature, "BlockParameter", cParameter);
}
