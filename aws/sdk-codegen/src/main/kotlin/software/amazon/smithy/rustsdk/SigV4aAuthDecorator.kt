/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

package software.amazon.smithy.rustsdk

import software.amazon.smithy.aws.traits.auth.SigV4Trait
import software.amazon.smithy.aws.traits.auth.UnsignedPayloadTrait
import software.amazon.smithy.model.knowledge.ServiceIndex
import software.amazon.smithy.model.shapes.OperationShape
import software.amazon.smithy.rust.codegen.client.smithy.ClientCodegenContext
import software.amazon.smithy.rust.codegen.client.smithy.customize.AuthSchemeOption
import software.amazon.smithy.rust.codegen.client.smithy.customize.ClientCodegenDecorator
import software.amazon.smithy.rust.codegen.client.smithy.generators.OperationCustomization
import software.amazon.smithy.rust.codegen.client.smithy.generators.OperationSection
import software.amazon.smithy.rust.codegen.client.smithy.generators.ServiceRuntimePluginCustomization
import software.amazon.smithy.rust.codegen.client.smithy.generators.ServiceRuntimePluginSection
import software.amazon.smithy.rust.codegen.core.rustlang.Writable
import software.amazon.smithy.rust.codegen.core.rustlang.rust
import software.amazon.smithy.rust.codegen.core.rustlang.rustTemplate
import software.amazon.smithy.rust.codegen.core.rustlang.writable
import software.amazon.smithy.rust.codegen.core.smithy.RuntimeType
import software.amazon.smithy.rust.codegen.core.util.hasEventStreamOperations
import software.amazon.smithy.rust.codegen.core.util.hasTrait
import software.amazon.smithy.rust.codegen.core.util.isInputEventStream
import software.amazon.smithy.rust.codegen.core.util.letIf

class SigV4aAuthDecorator : ClientCodegenDecorator {
    override val name: String get() = "SigV4aAuthDecorator"
    override val order: Byte = 0

    override fun authOptions(
        codegenContext: ClientCodegenContext,
        operationShape: OperationShape,
        baseAuthOptions: List<AuthSchemeOption>,
    ): List<AuthSchemeOption> = baseAuthOptions.letIf(codegenContext.smithyRuntimeMode.generateOrchestrator) {
        it + AuthSchemeOption.StaticAuthSchemeOption(SigV4Trait.ID) {
            rustTemplate(
                "#{scheme_id},",
                "scheme_id" to AwsRuntimeType.awsRuntime(codegenContext.runtimeConfig)
                    .resolve("auth::sigv4a::SCHEME_ID"),
            )
        }
    }

    override fun serviceRuntimePluginCustomizations(
        codegenContext: ClientCodegenContext,
        baseCustomizations: List<ServiceRuntimePluginCustomization>,
    ): List<ServiceRuntimePluginCustomization> =
        baseCustomizations.letIf(codegenContext.smithyRuntimeMode.generateOrchestrator) {
            it + listOf(V4aAuthServiceRuntimePluginCustomization(codegenContext))
        }

    override fun operationCustomizations(
        codegenContext: ClientCodegenContext,
        operation: OperationShape,
        baseCustomizations: List<OperationCustomization>,
    ): List<OperationCustomization> =
        baseCustomizations.letIf(codegenContext.smithyRuntimeMode.generateOrchestrator) {
            it + listOf(V4aAuthOperationCustomization(codegenContext))
        }
}

private class V4aAuthServiceRuntimePluginCustomization(private val codegenContext: ClientCodegenContext) :
    ServiceRuntimePluginCustomization() {
    private val runtimeConfig = codegenContext.runtimeConfig
    private val codegenScope by lazy {
        val awsRuntime = AwsRuntimeType.awsRuntime(runtimeConfig)
        arrayOf(
            "SigV4aAuthScheme" to awsRuntime.resolve("auth::sigv4a::SigV4aAuthScheme"),
            "SharedAuthScheme" to RuntimeType.smithyRuntimeApi(runtimeConfig).resolve("client::auth::SharedAuthScheme"),
        )
    }

    override fun section(section: ServiceRuntimePluginSection): Writable = writable {
        when (section) {
            is ServiceRuntimePluginSection.RegisterRuntimeComponents -> {
                val serviceHasEventStream = codegenContext.serviceShape.hasEventStreamOperations(codegenContext.model)
                if (serviceHasEventStream) {
                    // enable the aws-runtime `sign-eventstream` feature
                    addDependency(AwsCargoDependency.awsRuntime(runtimeConfig).withFeature("event-stream").toType().toSymbol())
                }
                section.registerAuthScheme(this) {
                    rustTemplate("#{SharedAuthScheme}::new(#{SigV4aAuthScheme}::new())", *codegenScope)
                }
            }

            else -> {}
        }
    }
}

private class V4aAuthOperationCustomization(private val codegenContext: ClientCodegenContext) : OperationCustomization() {
    private val runtimeConfig = codegenContext.runtimeConfig
    private val codegenScope by lazy {
        val awsRuntime = AwsRuntimeType.awsRuntime(runtimeConfig)
        arrayOf(
            "SigV4aOperationSigningConfig" to awsRuntime.resolve("auth::sigv4a::SigV4aOperationSigningConfig"),
            "SigningOptions" to awsRuntime.resolve("auth::sigv4a::SigningOptions"),
            "SignableBody" to AwsRuntimeType.awsSigv4(runtimeConfig).resolve("http_request::SignableBody"),
        )
    }
    private val serviceIndex = ServiceIndex.of(codegenContext.model)

    override fun section(section: OperationSection): Writable = writable {
        when (section) {
            is OperationSection.AdditionalRuntimePluginConfig -> {
                val authSchemes = serviceIndex.getEffectiveAuthSchemes(codegenContext.serviceShape, section.operationShape)
                if (authSchemes.containsKey(SigV4Trait.ID)) {
                    val unsignedPayload = section.operationShape.hasTrait<UnsignedPayloadTrait>()
                    val doubleUriEncode = unsignedPayload || !disableDoubleEncode(codegenContext.serviceShape)
                    val contentSha256Header = needsAmzSha256(codegenContext.serviceShape)
                    val normalizeUrlPath = !disableUriPathNormalization(codegenContext.serviceShape)
                    rustTemplate(
                        """
                        let mut signing_options = #{SigningOptions}::default();
                        signing_options.double_uri_encode = $doubleUriEncode;
                        signing_options.content_sha256_header = $contentSha256Header;
                        signing_options.normalize_uri_path = $normalizeUrlPath;
                        signing_options.payload_override = #{payload_override};

                        ${section.newLayerName}.store_put(#{SigV4aOperationSigningConfig} {
                            region_set: None,
                            service: None,
                            signing_options,
                        });
                        """,
                        *codegenScope,
                        "payload_override" to writable {
                            if (unsignedPayload) {
                                rustTemplate("Some(#{SignableBody}::UnsignedPayload)", *codegenScope)
                            } else if (section.operationShape.isInputEventStream(codegenContext.model)) {
                                // TODO(EventStream): Is this actually correct for all Event Stream operations?
                                rustTemplate("Some(#{SignableBody}::Bytes(&[]))", *codegenScope)
                            } else {
                                rust("None")
                            }
                        },
                    )
                }
            }

            else -> {}
        }
    }
}